# CLI reference

```text
gq [GLOBAL OPTIONS] <COMMAND>
```

## Command tree

```mermaid
flowchart TD
    GQ["gq"] --> RUN["gq run [OPTIONS] [COMMAND]..."]
    GQ --> LIST["gq list [OPTIONS]"]
    GQ --> TOPO["gq topo [OPTIONS]"]
```

## Global options

| Option          | Default | Meaning                                                                                                 |
| --------------- | ------- | ------------------------------------------------------------------------------------------------------- |
| `-v, --verbose` | Warn    | Increase log verbosity. `-v` is Info, `-vv` is Debug, and `-vvv` is Trace (further repeats stay Trace). |
| `-q, --quiet`   | off     | Disable all logging. Conflicts with `--verbose`. Data still prints.                                     |
| `-h, --help`    |         | Print help.                                                                                             |
| `-V, --version` |         | Print the version.                                                                                      |

`--verbose` and `--quiet` are global and may appear before or after the
subcommand. `RUST_LOG` is honored unless `--quiet` is set.

## gq run

```text
gq run [OPTIONS] [COMMAND]...
```

| Option                         | Default           | Meaning                                                                                                                      |
| ------------------------------ | ----------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `-i, --include <A,B-C,...>`    | all cards         | Keep only card indices in the listed inclusive ranges. Conflicts with `--exclude`.                                           |
| `-x, --exclude <A,B-C,...>`    | none              | Drop card indices in the listed inclusive ranges. Conflicts with `--include`.                                                |
| `--max-vram <SIZE_OR_PERCENT>` | 100 MiB used      | Require used VRAM to be at most the threshold. Conflicts with `--min-vram`.                                                  |
| `--min-vram <SIZE_OR_PERCENT>` | unset             | Require free VRAM to be at least the threshold. Conflicts with `--max-vram`.                                                 |
| `--max-utilization <PERCENT>`  | unset             | Require utilization to be at most the threshold. Alias: `--max-util`.                                                        |
| `--max-foreign-apps <COUNT>`   | 0                 | Require at most this many apps owned by other users. Apps with unreadable owners count as foreign.                           |
| `--max-any-apps <COUNT>`       | unset             | Require at most this many apps in total, regardless of owner.                                                                |
| `--probe-count <COUNT>`        | 1                 | Number of samples per probe round, one second apart. Alias: `--probe-cnt`.                                                   |
| `-k, --count <COUNT>`          | 1                 | Number of cards to select.                                                                                                   |
| `--strategy <STRATEGY>`        | `first`           | Card ordering: `first`, `min-used`, or `topology-aware`.                                                                     |
| `--poll <SECONDS>`             | 60                | Interval between probe rounds.                                                                                               |
| `--timeout <SECONDS>`          | 0                 | Exit 75 after this many seconds of waiting. Zero waits forever.                                                              |
| `--no-wait`                    | off               | Make one probe round and do not wait between rounds. Conflicts with explicit `--poll` or `--timeout`. Alias: `--no-waiting`. |
| `--dir <DIRECTORY>`            | current directory | Change directory before running the command. Aliases: `--change-directory`, `--cd`.                                          |
| `-e, --env <KEY=VALUE>`        | none              | Set an environment variable for the child. Repeatable.                                                                       |
| `--dry-run`                    | off               | Probe and select once, print the decision, and do not run a command.                                                         |
| `COMMAND...`                   |                   | Program and arguments. Optional only with `--dry-run`.                                                                       |

The command is executed directly, without a shell.
Use `sh -c '...'` when shell syntax is needed.
`CUDA_VISIBLE_DEVICES` and `GQ_CARDS` are reserved and are set by `gq` on the child.

Filter semantics are described in [filters.md](filters.md).
Wait and execution semantics are described in [run.md](run.md).

## gq list

```text
gq list [OPTIONS]
```

`gq list` probes the cards and prints their current status.
It accepts the same filter options as `gq run` (`--include`, `--exclude`, `--max-vram`, `--min-vram`, `--max-utilization`, `--max-foreign-apps`, `--max-any-apps`, and `--probe-count`).
A card that passes the idle predicate is marked `Available`; the others are marked `In use`.

| Option                  | Default  | Meaning                                    |
| ----------------------- | -------- | ------------------------------------------ |
| `-f, --format <FORMAT>` | `pretty` | Output format: `pretty`, `json`, or `csv`. |

## gq topo

```text
gq topo [OPTIONS]
```

`gq topo` queries `nvidia-smi topo -m` and prints the link matrix.
With one GPU it warns that the topology cannot be determined and exits 0.

| Option                  | Default  | Meaning                                    |
| ----------------------- | -------- | ------------------------------------------ |
| `-f, --format <FORMAT>` | `pretty` | Output format: `pretty`, `json`, or `csv`. |

## Output formats

`gq list` output has the columns or fields `index`, `memory_total_mib`, `memory_used_mib`, `memory_free_mib`, `utilization_percent`, and `status`.

- `pretty` prints a table.
- `json` prints a compact JSON array of objects.
- `csv` prints a header row followed by one row per card.

`gq topo` describes pairwise links.
The pretty matrix and JSON include both directions for each pair; CSV lists each unordered pair once.

- `pretty` prints a matrix with `H`, `M`, and `L` quality letters.
  On a terminal it uses color unless `NO_COLOR` is set.
- `json` prints a nested object keyed by card, with `link` and `quality` fields for every other card.
  The diagonal is omitted.
- `csv` prints the columns `card a`, `card b`, `link`, and `quality`.

`gq run` has no format option.
Logs go to stderr.
A dry-run decision goes to stdout.
A running child inherits gq's standard input, output, and error.

## Exit codes

| Code  | Meaning                                                                                                                                                                                              |
| ----- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0     | Success, including help and version output.                                                                                                                                                          |
| 1     | Internal or configuration error; probe error in `gq list`, `gq topo`, or `--dry-run`; no cards reported; topology matrix disagrees with the usage probe; working directory disappeared before spawn. |
| 64    | Usage error, such as an unknown flag, invalid value, impossible filter, or missing command without `--dry-run`.                                                                                      |
| 75    | Transient failure: no matching idle GPUs in a single round, no idle match in `--dry-run`, timeout expired, or probe failure under `--no-wait`. Retry later.                                          |
| 126   | Permission denied for the command or working directory, or the file is not a valid executable format.                                                                                                |
| 127   | Command or its `#!` interpreter was not found.                                                                                                                                                       |
| 128+n | Terminated by signal `n` (for example 143 is SIGTERM). `gq` also returns this when it forwarded the signal to the child.                                                                             |
| other | A child exit code not covered above, 0 through 255.                                                                                                                                                  |

## Logging

Logs are written to stderr with timestamps and levels.
Data and decisions are written to stdout.

- Default level is Warn: warnings and errors only, such as degraded or contradictory requests, probe failures, and candidate shortfalls.
- `-v` enables Info: idle thresholds, wait reasons, starting cards, and the effective candidate scope when `--include` or `--exclude` is set.
- `-vv` enables Debug: excluded indices, the unfiltered `N/N` candidate count, per-GPU idle decisions with the computed threshold and app counts, selection ranking and topology target tuples, and the next probe delay with the remaining timeout.
- `-vvv` enables Trace, including full `nvidia-smi` output.
  Further repeats stay at Trace.
- `-q` disables logging entirely.
- `RUST_LOG` can override the level unless `-q` is used.
- Color is used only when stderr is a terminal and `NO_COLOR` is unset.

Repeated wait rounds keep the same state at Debug; Info is reserved for the first wait and for changes in the reason.
A `--probe-count` above 60 warns once about the cost of sampling; a count whose one-second intervals cannot fit in `--timeout` also warns once, naming `--timeout` or `--probe-count` as the fix.
Unhandled errors print `Error: ...` to stderr unless `--quiet` is set.

## Completion and man page

Run `gq --help` for commands and `gq run --help` for every flag.
Shell completion is generated at build time or at runtime; the man page is generated at build time.

### Which file to use

| Shell      | Generated file | Install path                                                                 |
| ---------- | -------------- | ---------------------------------------------------------------------------- |
| Bash       | `gq.bash`      | `~/.local/share/bash-completion/completions/gq`                              |
| Zsh        | `_gq`          | a directory on `$fpath`, for example `~/.local/share/zsh/site-functions/_gq` |
| Fish       | `gq.fish`      | `~/.config/fish/completions/gq.fish`                                         |
| Elvish     | `gq.elv`       | `~/.config/elvish/lib/gq.elv`, then `use gq`                                 |
| PowerShell | `_gq.ps1`      | dot-source it from `$PROFILE`                                                |

The files are produced under `target/<profile>/completions` by a local build, with `debug` or `release` as the profile.
To install the Bash, Zsh, and Fish files from a release build:

```sh
install -Dm644 target/release/completions/gq.bash ~/.local/share/bash-completion/completions/gq
install -Dm644 target/release/completions/_gq ~/.local/share/zsh/site-functions/_gq
install -Dm644 target/release/completions/gq.fish ~/.config/fish/completions/gq.fish
```

For Elvish, copy `gq.elv` to `~/.config/elvish/lib/gq.elv` and add `use gq` to `rc.elv`.
For PowerShell, copy `_gq.ps1` anywhere and add `. path\to\_gq.ps1` to `$PROFILE`.

Homebrew installs the completions and the man page automatically.
Release archives contain `completions/` and `man/gq.1`, and the loose release assets include `gq.bash`, `_gq`, `gq.fish`, `gq.elv`, `_gq.ps1`, and `gq.1`.
Both `cargo binstall` and `cargo install` install only the binary, so either use dynamic completion or fetch the generated files from a release.

### Static versus dynamic completion

Static scripts are a build-time snapshot of the CLI.
They are self-contained and add little to shell startup, but they must be reinstalled after every upgrade.
Dynamic completion needs no generated files and always matches the installed binary, at the cost of one `gq` invocation per completion.
Add one of these lines to the shell startup file:

```sh
# bash
source <(GQ_COMPLETE=bash gq)

# zsh
source <(GQ_COMPLETE=zsh gq)

# fish
GQ_COMPLETE=fish gq | source
```

### Man page

Install the man page and read it:

```sh
install -Dm644 target/release/man/gq.1 ~/.local/share/man/man1/gq.1
mandb
man gq
```

`mandb` is only needed when new man pages are not found automatically.
Without installing, `man ./gq.1` reads the generated page in place.
