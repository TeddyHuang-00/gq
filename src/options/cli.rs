//! clap-facing command-line definitions.

use std::path::PathBuf;

use clap::{
    ArgAction, Args, Parser, Subcommand,
    builder::{Styles, styling::AnsiColor},
};

use super::{filter::Filter, output::OutputMode, select::Select};

/// Controls how to obtain the cards
#[derive(Args, Debug)]
pub struct Waiting {
    /// Interval between queries of GPU availability.
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 60,
        value_parser = clap::value_parser!(u64).range(1..=86400)
    )]
    pub poll: u64,
    /// Exit with code 75 after this many seconds; 0 waits forever.
    #[arg(long, value_name = "SECONDS", default_value_t = 0)]
    pub timeout: u64,
    /// Make one round of probes; exit with code 75 if no matching idle GPUs
    /// are found or the probe fails.
    #[arg(
        long,
        visible_alias = "no-waiting",
        conflicts_with_all = ["poll", "timeout"]
    )]
    pub no_wait: bool,
}

/// Controls the execution of commands
#[derive(Args, Debug)]
pub struct Execute {
    /// Change the working directory before executing the command
    #[arg(
        long,
        visible_aliases = ["change-directory", "cd"],
        value_name = "DIRECTORY"
    )]
    pub dir: Option<PathBuf>,
    /// Set an environment variable for the command; repeatable.
    ///
    /// `CUDA_VISIBLE_DEVICES` and `GQ_CARDS` are reserved: gq sets both for
    /// the command and rejects them here.
    #[arg(short = 'e', long, value_name = "KEY=VALUE", value_parser = parse_env)]
    pub env: Vec<(String, String)>,
    /// Probe and select once, print the decision, then exit; no command runs.
    #[arg(long)]
    pub dry_run: bool,
    /// Actual commands to run.
    ///
    /// You can optionally insert `--` before the commands for better
    /// readability.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

/// Queries and waits for available GPUs, then starts the job with selected
/// cards
#[derive(Args, Debug)]
pub struct SelectWaitAndExecute {
    #[command(flatten, next_help_heading = "Filtering cards")]
    pub filter: Filter,

    #[command(flatten, next_help_heading = "Selecting cards")]
    pub select: Select,

    #[command(flatten, next_help_heading = "Waiting for cards")]
    pub wait: Waiting,

    #[command(flatten, next_help_heading = "Execution")]
    pub execute: Execute,
}

/// Splits a `KEY=VALUE` environment assignment.
fn parse_env(value: &str) -> Result<(String, String), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got `{value}`"))?;
    if key.is_empty() {
        return Err("environment variable name must not be empty".to_owned());
    }
    if key.contains('\0') || value.contains('\0') {
        return Err("environment variable must not contain NUL".to_owned());
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// List GPUs and their status. Alternative to `nvidia-smi`
#[derive(Args, Debug)]
pub struct List {
    /// Output format
    #[arg(short, long, default_value = "pretty")]
    pub format: OutputMode,

    #[command(flatten, next_help_heading = "Filter")]
    pub filter: Filter,
}

/// Queries all GPUs and build the topology.
#[derive(Args, Debug)]
pub struct Topology {
    /// Output format
    #[arg(short, long, default_value = "pretty")]
    pub format: OutputMode,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Run(SelectWaitAndExecute),
    List(List),
    Topo(Topology),
}

#[derive(Debug, Parser)]
#[command(
    name = "gq",
    author,
    version,
    about = env!("CARGO_PKG_DESCRIPTION"),
    styles = Styles::styled()
        .header(AnsiColor::BrightGreen.on_default().bold())
        .usage(AnsiColor::Yellow.on_default())
        .literal(AnsiColor::Cyan.on_default())
        .placeholder(AnsiColor::BrightCyan.on_default())
)]
pub struct Cli {
    /// Set the log verbosity level
    ///
    /// - 0 = Warn (default)
    /// - 1 = Info
    /// - 2 = Debug
    /// - 3+ = Trace
    #[arg(
        short,
        long,
        action = ArgAction::Count,
        global = true,
        verbatim_doc_comment
    )]
    pub verbose: u8,

    /// Suppress all logging. Command, list, topology, and dry-run data still
    /// print.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_splits_once_and_rejects_malformed_assignments() {
        assert_eq!(
            parse_env("FOO=bar").ok(),
            Some(("FOO".to_owned(), "bar".to_owned()))
        );
        assert_eq!(
            parse_env("FOO=a=b").ok(),
            Some(("FOO".to_owned(), "a=b".to_owned()))
        );
        assert_eq!(
            parse_env("FOO=").ok(),
            Some(("FOO".to_owned(), String::new()))
        );
        assert!(parse_env("").is_err());
        assert!(parse_env("=bar").is_err());
        assert!(parse_env("FOO").is_err());
        assert!(parse_env("FOO=a\0b").is_err());
    }
}
