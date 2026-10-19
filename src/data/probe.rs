use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

/// Longest a single `nvidia-smi` probe may run.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a running probe checks for exit and the deadline.
const PROBE_POLL: Duration = Duration::from_millis(20);

/// Fails once `deadline` has passed, before starting or waiting for a probe.
pub(super) fn ensure_before_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        anyhow::bail!("the probe deadline passed before the probe finished");
    }
    Ok(())
}

/// Runs `nvidia-smi` (or `GQ_NVIDIA_SMI` when set) with the given arguments.
///
/// The child is killed after [`PROBE_TIMEOUT`] or when `deadline` passes,
/// whichever comes first. Signals are handled between probes, never during
/// one. Empty stdout is an error.
pub(super) fn smi(args: &[&str], deadline: Option<Instant>) -> Result<String> {
    smi_with(args, deadline, false)
}

/// Like [`smi`], but empty stdout is a valid answer, as when no compute app is
/// running.
pub(super) fn smi_allow_empty(args: &[&str], deadline: Option<Instant>) -> Result<String> {
    smi_with(args, deadline, true)
}

fn smi_with(args: &[&str], deadline: Option<Instant>, allow_empty: bool) -> Result<String> {
    let program = std::env::var("GQ_NVIDIA_SMI").unwrap_or_else(|_| "nvidia-smi".to_owned());
    if program.is_empty() {
        anyhow::bail!("GQ_NVIDIA_SMI is set but empty");
    }
    log::debug!("running `{program}` {args:?}");
    let limit = deadline.map_or(PROBE_TIMEOUT, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .min(PROBE_TIMEOUT)
    });
    if limit.is_zero() {
        anyhow::bail!("`{program}` not started: the probe deadline has already passed");
    }
    let started = Instant::now();
    let mut child = Command::new(&program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `{program}`"))?;
    loop {
        let finished = child
            .try_wait()
            .with_context(|| format!("cannot wait for `{program}`"))?;
        if finished.is_some() {
            break;
        }
        if started.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("`{program}` did not finish within {limit:?}; killed it");
        }
        std::thread::sleep(PROBE_POLL);
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("cannot read output of `{program}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`{program}` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout =
        String::from_utf8(output.stdout).context("nvidia-smi output is not valid UTF-8")?;
    if !allow_empty && stdout.trim().is_empty() {
        anyhow::bail!("`{program}` returned no output");
    }
    log::trace!("`{program}` {args:?} output:\n{stdout}");
    Ok(stdout)
}
