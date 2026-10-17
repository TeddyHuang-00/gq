pub mod list;
pub mod run;
pub mod topo;

use std::process::ExitCode;

use clap::{
    ArgAction, Parser, Subcommand,
    builder::{Styles, styling::AnsiColor},
};

/// Command outcome.
pub enum Exit {
    /// The command succeeded.
    Ok,
    /// The command finished with a specific exit code.
    #[allow(dead_code, reason = "reserved for commands that propagate a code")]
    Code(ExitCode),
}

impl Exit {
    pub const fn code(self) -> ExitCode {
        match self {
            Self::Ok => ExitCode::SUCCESS,
            Self::Code(code) => code,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Run(run::SelectWaitAndExecute),
    List(list::List),
    Topo(topo::Topology),
}

#[derive(Debug, Parser)]
#[command(
    name = "gq",
    author,
    version,
    about,
    styles = Styles::styled()
        .header(AnsiColor::BrightGreen.on_default().bold())
        .usage(AnsiColor::Yellow.on_default())
        .literal(AnsiColor::Cyan.on_default())
        .placeholder(AnsiColor::BrightCyan.on_default())
)]
pub struct Cli {
    /// Set the log verbosity level
    ///
    /// - 0 = Warning (default)
    /// - 1 = Info
    /// - 2 = Debug
    /// - 3 = Trace
    #[arg(
        short,
        long,
        action = ArgAction::Count,
        global = true,
        verbatim_doc_comment
    )]
    pub verbose: u8,

    /// Suppress all logging. List and topology data still prints.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}
