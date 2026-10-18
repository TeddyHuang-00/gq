# gq

`gq` is a GPU job runner for Linux hosts with `nvidia-smi`. It probes cards,
selects idle cards, and runs one command with `CUDA_VISIBLE_DEVICES` set.

## Build

```sh
cargo build --release
install -m 0755 target/release/gq ~/.local/bin/gq
```

A Rust toolchain with edition 2024 support is required. The host needs
`nvidia-smi`.

## Usage

```sh
# Current card status
gq list

# One card, any that is idle
gq run -- python train.py

# Two cards, prefer the best-connected pair
gq run -k 2 --strategy topology-aware -- torchrun --nproc_per_node=2 train.py

# Show the decision without running anything
gq run -i 2-5 -k 2 --dry-run

# Wait up to an hour for cards with at least 8 GiB free and low utilization
gq run --min-vram 8G --max-utilization 5 --poll 30 --timeout 3600 -- python train.py

# Extra environment, working directory, arguments after `--`
gq run -e WANDB_MODE=offline --dir ~/work/model -- python train.py --seed 45
```

The command is executed directly, without a shell. Use `sh -c '...'` when shell
syntax is required. `gq run --help` lists all flags.

Selection defaults:

- A card is idle when used VRAM is at most 100 MiB. `--min-vram` switches to a
  free-VRAM threshold; `--max-utilization` adds a utilization threshold.
- `--strategy first` takes the lowest idle indices, `min-used` takes the least
  used cards.
- `--strategy topology-aware` scores every candidate combination by link
  quality. It selects only a combination that matches the best score and is
  idle, so it waits instead of falling back to a worse-connected group. With
  `-k 1` it degenerates to `first` and does not query the topology.

`--timeout` bounds each probe, the one-time topology query, and the sleep
between probes; when it expires, `gq` exits 75.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Internal or configuration error; probe error in `gq list`, `gq topo`, or `--dry-run`; `--dir` vanished before spawn; topology matrix disagrees with the probe |
| 64 | Usage error or impossible request |
| 75 | No matching GPU after a round, timeout expired, or probe failure under `--no-wait`; `--dry-run` found no idle combination |
| 126 | Command or working directory not accessible, or the file is not a valid executable format |
| 127 | Command, or its `#!` interpreter, not found |
| 128+n | Killed by signal `n` (143 = TERM) |
| 0-255 | Otherwise the command's exit code |

## Concurrency

`gq` does not lock or claim cards. It probes, selects idle cards, sets
`CUDA_VISIBLE_DEVICES` and `GQ_CARDS`, and executes the command. Concurrent
invocations are not coordinated; use pueue or another scheduler when jobs must
not overlap.

`CUDA_VISIBLE_DEVICES` and `GQ_CARDS` are reserved: `gq` sets both to the
selected cards, and `--env` refuses to override them.

Environment override:

- `GQ_NVIDIA_SMI`: probe program, default `nvidia-smi`.

## Design notes

`plans/` contains local design notes and is gitignored, so it is not part of
this repository. When present:

- `plans/003-run-implementation.md`: current implementation contract.
- `plans/001-cli-api.md`: earlier CLI design, kept as history.
- `plans/002-deferred.md`: deferred features.
