# gq

`gq` is a GPU job runner for Linux hosts with `nvidia-smi`. It probes the
cards, filters and selects a group, sets `CUDA_VISIBLE_DEVICES` and `GQ_CARDS`,
and runs one command.

## What it is not

`gq` is not a queue or scheduler. It does not reserve cards or coordinate
concurrent invocations. Two `gq` processes can select the same idle cards. Put
`pueue` or another scheduler in front when jobs must not overlap.

## Build

A Rust toolchain with edition 2024 support is required.

```sh
cargo build --release
install -m 0755 target/release/gq ~/.local/bin/gq
```

## Quick start

```sh
# Current card status
gq list

# One idle card, run a command directly (no shell)
gq run -- python train.py

# Two cards for torchrun, preferring the best-connected pair
gq run -k 2 --strategy topology-aware -- torchrun --nproc_per_node=2 train.py

# Show the selection decision without running anything
gq run -i 2-5 -k 2 --dry-run

# Wait up to an hour for two cards with at least 8 GiB free and low utilization
gq run -k 2 --min-vram 8G --max-utilization 5 --poll 30 --timeout 3600 -- python train.py
```

Commands run directly, without a shell. Use `sh -c '...'` for shell syntax.
`gq run --help` lists every flag.

## Completion and man page

Build output includes completion scripts in `target/<profile>/completions` and
a man page at `target/<profile>/man/gq.1`. Install the man page with
`install -Dm644 target/release/man/gq.1 ~/.local/share/man/man1/gq.1`. See
[docs/cli.md](docs/cli.md#completion-and-man-page) for shell completion
install examples and dynamic completion with `GQ_COMPLETE`.

## Documentation

- [Run lifecycle and waiting](docs/run.md)
- [Filters and idle detection](docs/filters.md)
- [Topology-aware selection](docs/topology.md)
- [CLI, output, exit codes, and completion](docs/cli.md)

## License

All code are licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
