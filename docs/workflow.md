# Typical workflow

`gq` is the selection layer in a GPU workflow.
It answers one question at dispatch time: which cards should this command see.
It does not queue jobs and it does not reserve cards.
Queueing and coordination stay with the tools that already own them.

## Inspect the host

On a multi-GPU host, start with the link matrix and the current card state.

```sh
gq topo
gq list
```

`gq topo` prints the pairwise link matrix from `nvidia-smi topo -m`, with `H`, `M`, and `L` quality letters.
Use it to decide which cards belong in the same `-k` group.

`gq list` prints one row per card with total, used, and free memory, plus utilization and a status.
A card that passes the idle predicate is marked `Available`; the rest are marked `In use`.
The predicate is the same one `gq run` uses, so the listing previews what a default run would accept.

Both commands accept `-f json` and `-f csv` for scripts:

```sh
gq list -f json
gq topo -f csv
```

For one multi-card job, use the matrix through topology-aware selection:

```sh
gq run -k 2 --strategy topology-aware -- torchrun --nproc_per_node=2 train.py
```

Topology-aware selection scores whole `-k` groups and waits for the best-connected idle group instead of falling back to a weaker one.
[Topology-aware selection](topology.md) describes the scoring.

## Run one task

For a single task, run it directly.
No outer scheduler is needed.

```sh
gq run -- python train.py
```

`gq` probes, selects one idle card, sets `CUDA_VISIBLE_DEVICES` and `GQ_CARDS`, and runs the command without a shell.
It exits with the child status.

When the host is busy, let `gq` wait:

```sh
gq run -k 2 --min-vram 8G --max-util 5 --poll 30 --timeout 3600 -- python train.py
```

- `--min-vram 8G` requires at least 8 GiB free on each selected card. It replaces the default used-memory test of 100 MiB.
- `--max-util 5` requires utilization at or below 5 percent. The long form is `--max-utilization`.
- `--poll 30` probes every 30 seconds. The default is 60.
- `--timeout 3600` exits 75 after an hour of waiting. The default is 0, which waits forever.

The timeout bounds the wait, not the command.
Once the child starts, `gq` stays until it exits.

Preview the decision before committing:

```sh
gq run -i 2-5 -k 2 --dry-run
```

`--dry-run` makes one probe and one selection attempt.
It prints `would run on GPUs <indices>` and exits 0 on success, or prints `no GPUs match the current constraints` and exits 75.
stdout keeps the single result line; selection details go to the stderr log, visible with `-v` (idle rule) and `-vv` (per-card reasons).
It ignores `--poll` and does not wait.
`<indices>` is the comma-separated list that the probe selects, so the example above prints `would run on GPUs 2,3` only when cards 2 and 3 are the ones chosen.

## Queue many tasks

For more than one task, put a queue in front.
`pueue` owns queueing, priority, and slot management.
`task-spooler` (`ts`) follows the same model.
`gq` handles only the middle step for each job: choose cards when that job is dispatched.

A serialized pueue setup runs one job at a time.
Each job probes when it starts, so it sees the cards left by the previous job.

```sh
pueue parallel 1

pueue add 'gq run --min-vram 8G -- python train.py --config a.toml'
pueue add 'gq run --min-vram 8G -- python train.py --config b.toml'

pueue status
pueue log
```

pueue runs commands through the system shell, so quote each job as one argument.
`pueue parallel 1` keeps the pattern safe.
If the group allows more than one slot, two jobs can be dispatched at once and both can select the same idle cards.

## Concurrent invocations

`gq` does not reserve cards.
Selection is advisory: another process can take a card between the probe and the spawn, and two `gq` invocations can select the same idle cards.
The layer in front must prevent that.
There are two correct patterns.

**Serialize scheduler slots.** Set one slot, as in `pueue parallel 1` above.
Jobs run one at a time, so each probe sees the result of the previous job.

**Partition the cards.** Give each slot or group a disjoint `-i/--include` range.
On an eight-card host with two slots:

```sh
pueue parallel 2

pueue add 'gq run -i 0-3 -- python train.py --config a.toml'
pueue add 'gq run -i 4-7 -- python train.py --config b.toml'
```

Each job can only select inside its own half, so no coordination is needed between them.

Do not treat `--min-vram` or `--max-util` as a lock.
They observe the cards at probe time and reserve nothing.

## Role boundary

```mermaid
flowchart LR
    A["Job source or scheduler"] --> B["gq run"]
    B --> C["Probe and select"]
    C --> D["Child command"]
```

`gq` sits between a job source and the child command.
The source can be a shell, a Makefile, or a queue such as pueue.
`gq` chooses cards and spawns the child with `CUDA_VISIBLE_DEVICES` and `GQ_CARDS` set to the selected indices in ascending order.
Standard input, output, error, and the exit status pass through.

Because `gq` sets `CUDA_VISIBLE_DEVICES`, the child sees only the selected cards.
CUDA renumbers the visible cards from 0 in enumeration order, and PyTorch follows the same view: with `k` selected cards, `torch.cuda.device_count()` returns `k` and the program uses logical indices 0 through `k - 1`.
Write the program against logical indices starting at 0 and let `gq` map them to physical cards.
`GQ_CARDS` holds the physical indices for logs and records.

`gq` does not interpret the command, retry it, or change its arguments.
Anything beyond selection belongs to the layer above or to the child.

A cluster scheduler that already allocates GPUs, such as Slurm or Kubernetes, makes the advisory selection redundant.
The allocation is already the selection.
Run the job directly with the devices that the scheduler assigned.

## Exit status for wrappers

Two codes matter to a wrapper:

- 64 is a bad or impossible request: an unknown flag, invalid value, impossible filter, or a missing command without `--dry-run`. Do not retry it.
- 75 is transient: no idle match in a round or under `--dry-run`, an expired timeout, or a probe failure under `--no-wait`. Retry later.

The full table is in the [CLI reference](cli.md#exit-codes), including child statuses and signal exits.

`--no-wait` is the flag for scheduler-driven fail-fast dispatch.
It makes one probe round and returns 75 instead of holding the slot while it waits.
It conflicts with explicit `--poll` or `--timeout`.
Use it when the scheduler owns retries:

```sh
while true; do
    gq run --no-wait --min-vram 8G -- python train.py
    status=$?
    [ "$status" -eq 75 ] || exit "$status"
    sleep 300
done
```

A child can also exit 64 or 75 on its own, and `gq` passes that status through.
A wrapper cannot tell those apart from a selection failure.
