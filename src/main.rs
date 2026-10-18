mod commands;
mod options;
mod query;
mod units;
mod writer;

use std::{
    io::{IsTerminal, stderr},
    process::ExitCode,
};

use anyhow::Result;
use clap::Parser;
use simple_logger::SimpleLogger;

fn main() -> ExitCode {
    let args = match commands::Cli::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let code = if error.use_stderr() { 64 } else { 0 };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };

    let log_level = if args.quiet {
        log::LevelFilter::Off
    } else {
        match args.verbose {
            0 => log::LevelFilter::Info,
            1 => log::LevelFilter::Debug,
            2.. => log::LevelFilter::Trace,
        }
    };
    let colors = stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let logger = SimpleLogger::new().with_level(log_level);
    // RUST_LOG could override the quiet level, so only read it when not quiet.
    let logger = if args.quiet { logger } else { logger.env() };
    if let Err(error) = logger.with_colors(colors).init() {
        eprintln!("Error: {error}");
        return ExitCode::FAILURE;
    }

    match dispatch(&args) {
        Ok(exit) => exit.code(),
        Err(error) => {
            if !args.quiet {
                eprintln!("Error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &commands::Cli) -> Result<commands::Exit> {
    match &args.command {
        commands::Commands::List(command) => command.execute(),
        commands::Commands::Topo(command) => command.execute(),
        commands::Commands::Run(command) => command.execute(),
    }
}
