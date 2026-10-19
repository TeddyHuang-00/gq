# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-10-19

### Added

- **(run)** hint at -v details in dry-run output
- **(filter)** limit cards by compute apps
- **(topology)** calibrate link scores from config

### Changed

- **(run)** shorten the dry-run hint to point at -v
- **(tests)** split e2e suite into themed files
- **(layout)** nest run and topology modules under mod.rs
- **(filter)** split idle predicates and share card pipelines
- **(cli)** move clap definitions into options
- **(topology)** fold link score into data::topology
- **(data)** split query into per-source modules

### Performance

- **(data)** cache pid to uid lookups per app snapshot

### Fixed

- **(demo)** record the GIF from a fresh release build

### Documentation

- add a typical GPU workflow guide

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-10-18

### Added

- **(logging)** default to warnings and require -v for info
- **(logging)** explain filtering, selection, and wait decisions
- **(list)** show GB, bright bars with black text, centered headers
- **(list)** render memory and utilization bars in pretty output

### Changed

- write CSV through the csv crate and drop nom from units
- **(exec)** replace direct libc use with rustix and signal-hook constants

### Documentation

- cover install paths, completions, and a VHS demo

## [0.1.0] - 2026-10-18

### Added

- add completion/manpage generation
- **(run)** warn when topology-aware is used with one card
- **(run)** implement GPU selection, waiting, and execution
- implement list and topo commands

### Changed

- **(options)** split filter and rename pick to select
