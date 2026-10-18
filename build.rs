#![forbid(unsafe_code)]
#![allow(
    dead_code,
    reason = "the build script only needs the clap command tree"
)]

use std::{env, fs, io, path::PathBuf};

use clap::{CommandFactory, ValueEnum};
use clap_complete::{Shell, generate_to};
use clap_mangen::Man;

#[path = "src/commands/mod.rs"]
mod commands;
#[path = "src/options/mod.rs"]
mod options;
#[path = "src/query.rs"]
mod query;
#[path = "src/units/mod.rs"]
mod units;
#[path = "src/writer/mod.rs"]
mod writer;

fn main() -> io::Result<()> {
    println!("cargo::rerun-if-changed=src");

    let out_dir = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "OUT_DIR is not set"))?;
    let target_dir = out_dir
        .ancestors()
        .nth(3)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot locate target directory"))?;
    let completions = target_dir.join("completions");
    let man_dir = target_dir.join("man");
    fs::create_dir_all(&completions)?;
    fs::create_dir_all(&man_dir)?;

    let mut command = commands::Cli::command();
    for shell in Shell::value_variants() {
        generate_to(*shell, &mut command, "gq", &completions)?;
    }

    let mut manpage = Vec::new();
    Man::new(command).render(&mut manpage)?;
    fs::write(man_dir.join("gq.1"), manpage)?;

    Ok(())
}
