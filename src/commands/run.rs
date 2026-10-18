//! Select, wait to obtain, and execute commands smartly

mod exec;

use std::{
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use clap::Args;
use itertools::Itertools;

use super::{Exit, TEMPORARY_FAILURE, USAGE_ERROR};
use crate::{
    options::{
        filter::Filter,
        select::{Select, Strategy},
    },
    query::{AllUsage, Topology},
};

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

/// Outcome of one probe/select round.
enum Round {
    /// The run finished, or never will.
    Done(Exit),
    /// Nothing to run yet; wait and probe again.
    Wait,
}

impl SelectWaitAndExecute {
    /// Runs selection, waiting, and execution.
    pub fn execute(&self) -> Result<Exit> {
        if let Some(error) = self.validate() {
            log::error!("{error}");
            return Ok(Exit::Code(ExitCode::from(USAGE_ERROR)));
        }
        let count = usize::try_from(self.select.count).unwrap_or(usize::MAX);
        if matches!(self.select.strategy, Strategy::TopologyAware) && count == 1 {
            log::warn!(
                "`--strategy topology-aware` has no effect with `-k 1`; selecting the first idle GPU"
            );
        }
        let signals = exec::Signals::install()?;
        // The timeout is a wall-clock budget: start it before the one-time
        // topology query, so every `nvidia-smi` call is bounded by it.
        let start = Instant::now();
        let deadline = if self.wait.timeout == 0 {
            None
        } else {
            start.checked_add(Duration::from_secs(self.wait.timeout))
        };
        let topology = if matches!(self.select.strategy, Strategy::TopologyAware) && count > 1 {
            match Topology::query_with_deadline(deadline) {
                Ok(topology) => Some(topology),
                Err(error) => {
                    if let Some(signal) = signals.take() {
                        return Ok(exec::signal_exit(signal));
                    }
                    if let Some(exit) = self.check_timeout(start) {
                        return Ok(exit);
                    }
                    return Err(error.context("cannot read GPU topology from `nvidia-smi topo -m`"));
                }
            }
        } else {
            None
        };
        if let Some(signal) = signals.take() {
            return Ok(exec::signal_exit(signal));
        }
        if let Some(exit) = self.check_timeout(start) {
            return Ok(exit);
        }

        let mut validated = false;
        loop {
            if let Some(exit) = self.check_timeout(start) {
                return Ok(exit);
            }
            let first = !validated;
            let probed = AllUsage::query_with_deadline(self.filter.probe_count, deadline);
            // A signal beats both the timeout and the probe result: a caller
            // that cancelled must not see a retryable code or a decision.
            if let Some(signal) = signals.take() {
                return Ok(exec::signal_exit(signal));
            }
            if let Some(exit) = self.check_timeout(start) {
                return Ok(exit);
            }
            let round = match probed {
                Ok(usage) => {
                    validated = true;
                    self.run_round(&usage, count, topology.as_ref(), first, &signals)?
                }
                Err(error) if self.execute.dry_run => return Err(error),
                Err(error) if self.wait.no_wait => {
                    log::warn!(
                        "probe failed: {error:#}; --no-wait is set, so no GPU decision was made"
                    );
                    return Ok(Exit::Code(ExitCode::from(TEMPORARY_FAILURE)));
                }
                Err(error) => {
                    log::warn!("probe failed: {error:#}; retrying");
                    Round::Wait
                }
            };
            if let Round::Done(exit) = round {
                return Ok(exit);
            }
            if self.wait.no_wait {
                log::warn!("no matching GPUs available after one attempt");
                return Ok(Exit::Code(ExitCode::from(TEMPORARY_FAILURE)));
            }
            if let Some(exit) = self.check_timeout(start) {
                return Ok(exit);
            }
            if let Some(signal) = signals.sleep(self.next_sleep(start)) {
                return Ok(exec::signal_exit(signal));
            }
        }
    }

    /// Validates the request before any probe; returns a user-facing problem.
    fn validate(&self) -> Option<String> {
        if !self.execute.dry_run && self.execute.command.is_empty() {
            return Some("no command to run; pass it after `--` or use --dry-run".to_owned());
        }
        if self.execute.command.first().is_some_and(String::is_empty) {
            return Some("the command's program name must not be empty".to_owned());
        }
        if let Some(dir) = &self.execute.dir
            && !dir.is_dir()
        {
            return Some(format!("--dir {} is not a directory", dir.display()));
        }
        for (key, _) in &self.execute.env {
            if key == "CUDA_VISIBLE_DEVICES" || key == "GQ_CARDS" {
                return Some(format!(
                    "--env {key} is reserved: gq sets it for the command"
                ));
            }
        }
        None
    }

    /// Handles one successful probe: select and execute.
    fn run_round(
        &self,
        usage: &AllUsage,
        count: usize,
        topology: Option<&Topology>,
        first: bool,
        signals: &exec::Signals,
    ) -> Result<Round> {
        let present = usage.iter().map(|(index, _)| index).collect::<Vec<_>>();
        if present.is_empty() {
            bail!("nvidia-smi reported no cards");
        }
        if first {
            self.filter.warn_absent(&present);
        }
        let candidates = usage
            .iter()
            .filter(|(index, _)| self.filter.accepts_index(*index))
            .collect::<Vec<_>>();
        if let Some(topology) = topology {
            let dimensions = topology.dimensions();
            if let Some(index) = candidates
                .iter()
                .map(|(index, _)| *index)
                .find(|&index| index >= dimensions)
            {
                bail!(
                    "GPU index {index} is outside the {dimensions}x{dimensions} topology matrix; \
                     `nvidia-smi topo -m` disagrees with `nvidia-smi --query-gpu`"
                );
            }
        }
        let problem = if candidates.is_empty() {
            Some(format!("no GPUs match the filter; present: {present:?}"))
        } else if candidates.len() < count {
            Some(format!(
                "requested {count} GPUs but only {} match the filter",
                candidates.len()
            ))
        } else {
            None
        };
        if let Some(problem) = problem {
            if first {
                log::error!("{problem}");
                return Ok(Round::Done(Exit::Code(ExitCode::from(USAGE_ERROR))));
            }
            log::warn!("{problem}; waiting");
            return Ok(Round::Wait);
        }

        let idle = candidates
            .iter()
            .filter(|(_, stats)| self.filter.accepts_usage(*stats))
            .map(|(index, _)| *index)
            .collect::<Vec<_>>();
        let Some(selected) = self.select.select(&candidates, &idle, topology) else {
            if self.execute.dry_run {
                println!("no GPUs match the current constraints");
                return Ok(Round::Done(Exit::Code(ExitCode::from(TEMPORARY_FAILURE))));
            }
            log::info!(
                "waiting: {}/{} candidate GPUs idle; need {count}",
                idle.len(),
                candidates.len()
            );
            return Ok(Round::Wait);
        };
        log::debug!("candidates {present:?}; idle {idle:?}; selected {selected:?}");
        if self.execute.dry_run {
            println!("would run on GPUs {}", join_cards(&selected));
            return Ok(Round::Done(Exit::Ok));
        }
        if let Some(signal) = signals.take() {
            return Ok(Round::Done(exec::signal_exit(signal)));
        }
        log::info!("starting on GPUs {}", join_cards(&selected));
        exec::run(
            &self.execute.command,
            self.execute.dir.as_deref(),
            &self.execute.env,
            &selected,
            signals,
        )
        .map(Round::Done)
    }

    /// Returns the timeout exit once the deadline has passed.
    fn check_timeout(&self, start: Instant) -> Option<Exit> {
        let deadline = Duration::from_secs(self.wait.timeout);
        if self.wait.timeout == 0 || start.elapsed() < deadline {
            return None;
        }
        log::warn!(
            "timed out after {}s waiting for available GPUs",
            self.wait.timeout
        );
        Some(Exit::Code(ExitCode::from(TEMPORARY_FAILURE)))
    }

    /// Sleeps for the poll interval, capped by the remaining timeout.
    fn next_sleep(&self, start: Instant) -> Duration {
        let poll = Duration::from_secs(self.wait.poll);
        if self.wait.timeout == 0 {
            return poll;
        }
        let deadline = Duration::from_secs(self.wait.timeout);
        poll.min(deadline.saturating_sub(start.elapsed()))
    }
}

/// Formats cards as a comma-separated list.
fn join_cards(cards: &[usize]) -> String {
    cards.iter().join(",")
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
        assert_eq!(join_cards(&[1, 2, 3]), "1,2,3");
    }
}
