//! Select, wait to obtain, and execute commands smartly

mod exec;
mod selection;

use std::{
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use itertools::Itertools;

use super::{Exit, TEMPORARY_FAILURE, USAGE_ERROR};
use crate::{
    data::{AllUsage, LinkProfile, Topology},
    options::{cli::SelectWaitAndExecute, select::Strategy},
};

/// Outcome of one probe/select round.
enum Round {
    /// The run finished, or never will.
    Done(Exit),
    /// Nothing to run yet; wait and probe again.
    Wait,
}

/// Remembers the last wait reason so repeated rounds log at debug.
#[derive(Default)]
struct WaitLog {
    last: Option<String>,
}

impl WaitLog {
    /// Returns whether `state` differs from the previous round.
    fn changed(&mut self, state: &str) -> bool {
        let changed = self.last.as_deref() != Some(state);
        if changed {
            self.last = Some(state.to_owned());
        }
        changed
    }
}

impl SelectWaitAndExecute {
    /// Runs selection, waiting, and execution.
    pub fn execute(&self) -> Result<Exit> {
        if let Some(error) = self.validate() {
            log::error!("{error}");
            return Ok(Exit::Code(ExitCode::from(USAGE_ERROR)));
        }
        self.warn_slow_probe();
        let count = usize::try_from(self.select.count).unwrap_or(usize::MAX);
        if matches!(self.select.strategy, Strategy::TopologyAware) && count == 1 {
            log::warn!(
                "`--strategy topology-aware` has no effect with `-k 1`; selecting the first idle \
                 GPU (use `-k 2` or more, or `--strategy first`)"
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
        let profile = self.link_profile(count)?;
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
        let mut wait_log = WaitLog::default();
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
                    self.run_round(
                        &usage,
                        topology.as_ref(),
                        &profile,
                        first,
                        &mut wait_log,
                        &signals,
                    )?
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
                return Ok(Exit::Code(ExitCode::from(TEMPORARY_FAILURE)));
            }
            if let Some(exit) = self.check_timeout(start) {
                return Ok(exit);
            }
            let sleep = self.next_sleep(start);
            log::debug!(
                "next probe in {:.1}s{}",
                sleep.as_secs_f64(),
                self.remaining_note(start)
            );
            if let Some(signal) = signals.sleep(sleep) {
                return Ok(exec::signal_exit(signal));
            }
        }
    }

    /// Loads the link profile only when topology-aware selection uses it.
    fn link_profile(&self, count: usize) -> Result<LinkProfile> {
        if matches!(self.select.strategy, Strategy::TopologyAware) && count > 1 {
            LinkProfile::load()
        } else {
            Ok(LinkProfile::default())
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
            return Some(format!(
                "--dir {} is not a directory; pass an existing directory",
                dir.display()
            ));
        }
        for (key, _) in &self.execute.env {
            if key == "CUDA_VISIBLE_DEVICES" || key == "GQ_CARDS" {
                return Some(format!(
                    "--env {key} is reserved: gq sets it for the command, remove it"
                ));
            }
        }
        None
    }

    /// Warns when sampling alone cannot fit in the timeout budget.
    fn warn_slow_probe(&self) {
        if self.wait.timeout == 0 || self.filter.probe_count <= 1 {
            return;
        }
        let sampling = u64::from(self.filter.probe_count - 1);
        if sampling >= self.wait.timeout {
            log::warn!(
                "`--probe-count {}` cannot fit in `--timeout {}s`; raise --timeout or lower \
                 --probe-count",
                self.filter.probe_count,
                self.wait.timeout
            );
        }
    }

    /// Handles one successful probe: select and execute.
    fn run_round(
        &self,
        usage: &AllUsage,
        topology: Option<&Topology>,
        profile: &LinkProfile,
        first: bool,
        wait_log: &mut WaitLog,
        signals: &exec::Signals,
    ) -> Result<Round> {
        let count = usize::try_from(self.select.count).unwrap_or(usize::MAX);
        let present = usage.iter().map(|(index, _)| index).collect::<Vec<_>>();
        if present.is_empty() {
            bail!("nvidia-smi reported no cards");
        }
        let candidates = self.filter.candidates(usage);
        if first {
            self.filter.warn_absent(&present);
            let indices = candidates
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>();
            self.filter.log_candidates(&present, &indices);
            self.filter.log_idle_rule();
        }
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
            Some(format!(
                "no GPUs match the filter; present: {present:?}; check --include/--exclude"
            ))
        } else if candidates.len() < count {
            let hint = if self.filter.has_index_filter() {
                "lower -k or widen --include/--exclude"
            } else {
                "lower -k"
            };
            Some(format!(
                "requested {count} GPUs but only {} candidates are available; {hint} (present: {present:?})",
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
            if wait_log.changed(&problem) {
                log::warn!("{problem}; waiting");
            } else {
                log::debug!("{problem}; still waiting");
            }
            return Ok(Round::Wait);
        }

        for (index, stats) in &candidates {
            self.filter.log_idle(*index, *stats);
        }
        let idle = self.filter.idle_cards(&candidates);
        let Some(selected) = self.select.select(&candidates, &idle, topology, profile) else {
            return Ok(self
                .handle_selection_miss(count, idle.len(), candidates.len(), topology, wait_log)
                .map_or(Round::Wait, Round::Done));
        };
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

    /// Logs a failed selection and reports whether the round must end.
    fn handle_selection_miss(
        &self,
        count: usize,
        idle: usize,
        candidates: usize,
        topology: Option<&Topology>,
        wait_log: &mut WaitLog,
    ) -> Option<Exit> {
        // `run` only passes `count > 1` topology-aware selection a matrix; a
        // miss with enough idle cards is therefore a target mismatch.
        let topology_miss = matches!(self.select.strategy, Strategy::TopologyAware)
            && count > 1
            && idle >= count
            && topology.is_some();
        let state = if topology_miss {
            format!(
                "no idle group matches the topology target ({idle} of {candidates} cards idle, \
                 need {count})"
            )
        } else {
            format!("{idle}/{candidates} candidate GPUs idle; need {count}")
        };
        if self.execute.dry_run {
            if topology_miss {
                log::info!(
                    "{state}; `--strategy topology-aware` does not fall back; use \
                     `--strategy first` to accept the best available group"
                );
            }
            println!("no GPUs match the current constraints");
            return Some(Exit::Code(ExitCode::from(TEMPORARY_FAILURE)));
        }
        let advice = match (topology_miss, self.wait.no_wait) {
            (false, _) => "",
            (true, true) => {
                "; wait for a topology-compatible group by dropping --no-wait, or use \
                 `--strategy first`"
            }
            (true, false) => "; wait for a topology-compatible group, or use `--strategy first`",
        };
        if self.wait.no_wait {
            log::warn!(
                "no matching GPUs available after one attempt: {state}; --no-wait is set{advice}"
            );
        } else if wait_log.changed(&state) {
            log::info!("waiting: {state}{}{advice}", self.wait_note());
        } else {
            log::debug!("waiting: {state}");
        }
        None
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

    /// Describes the poll and timeout settings for the first wait log.
    fn wait_note(&self) -> String {
        let timeout = if self.wait.timeout == 0 {
            "no timeout".to_owned()
        } else {
            format!("timeout {}s", self.wait.timeout)
        };
        format!(" (poll {}s, {timeout})", self.wait.poll)
    }

    /// Describes the remaining timeout budget for per-round debug logs.
    fn remaining_note(&self, start: Instant) -> String {
        if self.wait.timeout == 0 {
            return " (no timeout)".to_owned();
        }
        let remaining = Duration::from_secs(self.wait.timeout).saturating_sub(start.elapsed());
        format!(" ({:.1}s left)", remaining.as_secs_f64())
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
    fn wait_log_reports_only_state_changes() {
        let mut log = WaitLog::default();
        assert!(log.changed("2/4 candidate GPUs idle; need 3"));
        assert!(!log.changed("2/4 candidate GPUs idle; need 3"));
        assert!(log.changed("3/4 candidate GPUs idle; need 3"));
    }

    #[test]
    fn join_cards_formats_a_comma_separated_list() {
        assert_eq!(join_cards(&[1, 2, 3]), "1,2,3");
    }
}
