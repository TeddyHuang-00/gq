# gq

`gq` is a GPU job runner for Linux hosts with `nvidia-smi`.
It probes the cards, filters and selects a group, sets `CUDA_VISIBLE_DEVICES` and `GQ_CARDS`, and runs one command.

![gq demo](https://raw.githubusercontent.com/TeddyHuang-00/gq/refs/heads/main/assets/demo.gif)

## What it is not

`gq` is not a queue or scheduler. It does not reserve cards or coordinate concurrent invocations.
Two `gq` processes can select the same idle cards.
Put `pueue` or another scheduler in front when jobs must not overlap.

## Install

`gq` is intended to only work on Linux with `nvidia-smi`. So only installation methods for linux is shown.

### `cargo binstall`

The quickest path to a prebuilt binary is [cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall gq
```

If cargo-binstall is not installed, bootstrap it with `cargo install cargo-binstall`.

### Linuxbrew

```sh
brew install teddyhuang-00/app/gq
```

The formula installs the binary, the man page, and all shell completions.

### Pre-built binaries

GitHub release archives are static musl binaries for x86_64 and aarch64:

```sh
curl -LO https://github.com/TeddyHuang-00/gq/releases/latest/download/gq-x86_64-unknown-linux-musl.tar.xz
tar xf gq-x86_64-unknown-linux-musl.tar.xz
install -Dm755 gq-x86_64-unknown-linux-musl/gq ~/.local/bin/gq
```

Each archive also contains `completions/` and `man/gq.1`, and each has a `.sha256` file alongside it.
See [docs/cli.md](docs/cli.md#completion-and-man-page) for where to install the completion files.

### Build from source

Building from source requires a Rust toolchain with edition 2024 support.
To install a published release:

```sh
cargo install --locked gq
```

Or clone the repository and build it:

```sh
cargo build --release
install -Dm755 target/release/gq ~/.local/bin/gq
```

A local build writes completion scripts to `target/release/completions` and the man page to `target/release/man/gq.1`.

## Quick start

```sh
# Check topology (intended for multi-GPUs)
gq topo

# Current card status
gq list

# One idle card, run a command directly (no shell)
gq run -- python train.py

# Two cards for torchrun, preferring the best-connected pair
gq run -k 2 --strategy topology-aware -- torchrun --nproc_per_node=2 train.py

# Show the selection decision without running anything
gq run -i 2-5 -k 2 --dry-run

# Wait up to an hour for two cards with at least 8 GiB free and low utilization
gq run -k 2 --min-vram 8G --max-util 5 --poll 30 --timeout 3600 -- python train.py
```

Commands run directly, without a shell.
Use `sh -c '...'` for shell syntax.
`gq run --help` lists every flag.

## Completion and man page

A local build writes completion scripts to `target/release/completions` and a man page to `target/release/man/gq.1`.
[docs/cli.md](docs/cli.md#completion-and-man-page) maps each shell to its generated file and install path,
and covers dynamic completion with `GQ_COMPLETE`.

## Documentation

- [Run lifecycle and waiting](docs/run.md)
- [Filters and idle detection](docs/filters.md)
- [Topology-aware selection](docs/topology.md)
- [CLI, output, exit codes, and completion](docs/cli.md)

## License

All code are licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
