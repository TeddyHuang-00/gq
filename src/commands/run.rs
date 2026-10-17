//! Select, wait to obtain, and execute commands smartly

use std::path::PathBuf;

use clap::Args;

use crate::options::select::{Filter, Pick};

/// Controls how to obtain the cards
#[derive(Args, Debug)]
pub struct Wait {
    /// Interval between queries of GPU availability.
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    pub poll: usize,
    /// Timeout before giving up trying and return a failure signal.
    ///
    /// 0 means never timeout.
    #[arg(long, value_name = "SECONDS", default_value_t = 0)]
    pub timeout: usize,
    /// Skip the waiting and only try once immediately.
    #[arg(
        long,
        visible_alias = "no-wait",
        conflicts_with_all = ["poll", "timeout"]
    )]
    pub no_waiting: bool,
}

/// Controls the execution of commands
#[derive(Args, Debug)]
pub struct Execute {
    /// Change the working directory before executing the command
    #[arg(long, visible_alias = "cd", value_name = "DIRECTORY")]
    pub change_directory: Option<PathBuf>,
    /// Dry run only. Runs through everything until the actual execution of the
    /// command.
    #[arg(long)]
    pub dry_run: bool,
    /// Actual commands to run.
    ///
    /// You can optionally insert `--` before the commands for better
    /// readability.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

/// Queries and wait for available GPUs, then start the job with selected cards
#[derive(Args, Debug)]
pub struct SelectWaitAndExecute {
    #[command(flatten, next_help_heading = "Choosing cards")]
    pub filter: Filter,

    #[command(flatten, next_help_heading = "Picking cards")]
    pub pick: Pick,

    #[command(flatten, next_help_heading = "Waiting for cards")]
    pub wait: Wait,

    #[command(flatten, next_help_heading = "Execution")]
    pub execute: Execute,
}
