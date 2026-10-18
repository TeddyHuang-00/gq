//! Child process execution with reserved GPU environment and signal forwarding.

use std::{
    io,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command, ExitCode, ExitStatus},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, kill_process};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

use crate::commands::Exit;

/// How long the wait loop may go without reaping the child.
const POLL: Duration = Duration::from_millis(50);
/// Longest single sleep slice, so a signal is noticed promptly.
const SLICE: Duration = Duration::from_millis(100);

/// ENOEXEC, the same number on every unix platform gq runs on.
const ENOEXEC: i32 = 8;

/// SIGHUP/SIGINT/SIGTERM captured into one flag. Process-global: install once.
pub struct Signals {
    flag: Arc<AtomicUsize>,
}

impl Signals {
    /// Installs handlers for SIGHUP, SIGINT, and SIGTERM.
    ///
    /// # Errors
    ///
    /// Registration failure from the signal-hook registry.
    pub fn install() -> Result<Self> {
        let signals = Self {
            flag: Arc::new(AtomicUsize::new(0)),
        };
        for signal in [SIGHUP, SIGINT, SIGTERM] {
            let value = usize::try_from(signal)
                .with_context(|| format!("invalid signal number {signal}"))?;
            signal_hook::flag::register_usize(signal, Arc::clone(&signals.flag), value)
                .with_context(|| format!("cannot install handler for signal {signal}"))?;
        }
        Ok(signals)
    }

    /// Takes and clears the pending signal, if any.
    pub fn take(&self) -> Option<i32> {
        let raw = self.flag.swap(0, Ordering::SeqCst);
        i32::try_from(raw).ok().filter(|&signal| signal != 0)
    }

    /// Sleeps up to `duration` in 100 ms slices, returning early on a signal.
    pub fn sleep(&self, duration: Duration) -> Option<i32> {
        let deadline = Instant::now() + duration;
        loop {
            if let Some(signal) = self.take() {
                return Some(signal);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            std::thread::sleep(left.min(SLICE));
        }
    }
}

/// Runs `command` with inherited stdio.
///
/// `CUDA_VISIBLE_DEVICES` and `GQ_CARDS` are always set to the ascending card
/// list, overriding whatever `env` says.
///
/// # Errors
///
/// An empty command, a spawn failure other than a missing or non-executable
/// program, or an error from `wait`.
pub fn run(
    command: &[String],
    dir: Option<&Path>,
    env: &[(String, String)],
    cards: &[usize],
    signals: &Signals,
) -> Result<Exit> {
    let mut prepared = prepare(command, dir, env, cards)?;
    let mut child = match prepared.spawn() {
        Ok(child) => child,
        Err(err) => {
            if err.raw_os_error() == Some(ENOEXEC) {
                log::error!(
                    "cannot start `{}`: {err}; the file is not an executable format",
                    command[0]
                );
                return Ok(Exit::Code(ExitCode::from(126)));
            }
            if err.kind() == io::ErrorKind::NotFound
                && let Some(dir) = dir
                && !dir.is_dir()
            {
                log::error!(
                    "cannot start `{}`: working directory {} disappeared: {err}",
                    command[0],
                    dir.display()
                );
                return Ok(Exit::Code(ExitCode::from(1)));
            }
            return match err.kind() {
                io::ErrorKind::NotFound => {
                    log::error!("cannot start `{}`: {err}", command[0]);
                    Ok(Exit::Code(ExitCode::from(127)))
                }
                io::ErrorKind::PermissionDenied => {
                    log::error!("cannot start `{}`: {err}", command[0]);
                    Ok(Exit::Code(ExitCode::from(126)))
                }
                _ => Err(err).with_context(|| format!("cannot start `{}`", command[0])),
            };
        }
    };

    let mut forwarded = None;
    let status = loop {
        // Reap first: this shrinks the signal-vs-exit race window in which a
        // forwarded signal could turn the child's own code into 128+n. It
        // does not eliminate it: a signal can still hit a zombie between this
        // try_wait and the kill below.
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                return Err(err).with_context(|| format!("cannot wait for `{}`", command[0]));
            }
        }
        if let Some(signal) = signals.take() {
            note_forwarded(&child, signal, &mut forwarded);
        }
        // ponytail: 50 ms poll; pidfd_open would be event-driven but adds a
        // dependency, and 20 wakeups/s is negligible next to a GPU job.
        if let Some(signal) = signals.sleep(POLL) {
            note_forwarded(&child, signal, &mut forwarded);
        }
    };

    forwarded.map_or_else(
        || Ok(exit_from_status(status)),
        |signal| Ok(signal_exit(signal)),
    )
}

/// Maps a signalled process to the conventional 128+signal exit status.
#[must_use]
pub fn signal_exit(signal: i32) -> Exit {
    let Ok(signal) = u8::try_from(signal) else {
        return Exit::Code(ExitCode::from(1));
    };
    Exit::Code(ExitCode::from(signal.saturating_add(128)))
}

fn exit_from_status(status: ExitStatus) -> Exit {
    status.code().map_or_else(
        || signal_exit(status.signal().unwrap_or(0)),
        |code| Exit::Code(ExitCode::from(u8::try_from(code).unwrap_or(1))),
    )
}

fn prepare(
    command: &[String],
    dir: Option<&Path>,
    env: &[(String, String)],
    cards: &[usize],
) -> Result<Command> {
    let Some(program) = command.first() else {
        bail!("no command to run");
    };
    let mut cmd = Command::new(program);
    cmd.args(&command[1..])
        .envs(env.iter().map(|(key, value)| (key, value)));
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let mut cards = cards.to_vec();
    cards.sort_unstable();
    let joined = cards
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    cmd.env("CUDA_VISIBLE_DEVICES", &joined)
        .env("GQ_CARDS", &joined);
    Ok(cmd)
}

fn note_forwarded(child: &Child, signal: i32, first: &mut Option<i32>) {
    if forward(child, signal) {
        first.get_or_insert(signal);
    }
}

/// Forwards `signal` to `child`; returns whether the kernel accepted it.
fn forward(child: &Child, raw: i32) -> bool {
    let Some(signal) = Signal::from_named_raw(raw) else {
        log::warn!(
            "cannot forward unknown signal {raw} to child {}",
            child.id()
        );
        return false;
    };
    match kill_process(Pid::from_child(child), signal) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("cannot forward signal {raw} to child {}: {err}", child.id());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn test_signals() -> Signals {
        Signals {
            flag: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        std::env::temp_dir().join(format!("gq-exec-{}-{nanos}-{name}", std::process::id()))
    }

    #[test]
    fn prepares_command_with_reserved_gpu_env_and_cwd() -> Result<()> {
        let dir = Path::new("/tmp");
        let env = vec![
            ("FOO".to_owned(), "bar".to_owned()),
            ("CUDA_VISIBLE_DEVICES".to_owned(), "0".to_owned()),
            ("GQ_CARDS".to_owned(), "9".to_owned()),
        ];
        let cmd = prepare(
            &["prog".to_owned(), "--flag".to_owned()],
            Some(dir),
            &env,
            &[5, 3],
        )?;

        assert_eq!(cmd.get_program(), OsStr::new("prog"));
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), [OsStr::new("--flag")]);
        assert_eq!(cmd.get_current_dir(), Some(dir));

        let value = |key: &str| {
            cmd.get_envs()
                .find(|(name, _)| *name == OsStr::new(key))
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
        };
        assert_eq!(value("FOO").as_deref(), Some("bar"));
        assert_eq!(value("CUDA_VISIBLE_DEVICES").as_deref(), Some("3,5"));
        assert_eq!(value("GQ_CARDS").as_deref(), Some("3,5"));
        Ok(())
    }

    #[test]
    fn propagates_child_exit_code() -> Result<()> {
        let command = ["/bin/sh", "-c", "exit 42"].map(str::to_owned);
        let exit = run(&command, None, &[], &[], &test_signals())?;
        assert_eq!(exit.code(), ExitCode::from(42));
        Ok(())
    }

    #[test]
    fn maps_missing_program_to_127() -> Result<()> {
        let command = ["gq-definitely-not-a-real-program"].map(str::to_owned);
        let exit = run(&command, None, &[], &[], &test_signals())?;
        assert_eq!(exit.code(), ExitCode::from(127));
        Ok(())
    }

    #[test]
    fn maps_permission_denied_to_126() -> Result<()> {
        let path = temp_path("no-exec");
        fs::write(&path, "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

        let command = [path.to_string_lossy().into_owned()];
        let exit = run(&command, None, &[], &[], &test_signals());

        fs::remove_file(&path)?;
        assert_eq!(exit?.code(), ExitCode::from(126));
        Ok(())
    }

    #[test]
    fn maps_exec_format_error_to_126() -> Result<()> {
        let path = temp_path("bad-format");
        fs::write(&path, "not an executable format\n")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;

        let command = [path.to_string_lossy().into_owned()];
        let exit = run(&command, None, &[], &[], &test_signals());

        fs::remove_file(&path)?;
        assert_eq!(exit?.code(), ExitCode::from(126));
        Ok(())
    }

    #[test]
    fn maps_missing_working_directory_to_1() -> Result<()> {
        let command = ["true"].map(str::to_owned);
        let missing = temp_path("missing-dir");
        let exit = run(&command, Some(&missing), &[], &[], &test_signals())?;
        assert_eq!(exit.code(), ExitCode::from(1));
        Ok(())
    }

    #[test]
    fn signal_exit_adds_128() {
        assert_eq!(signal_exit(SIGTERM).code(), ExitCode::from(143));
        assert_eq!(signal_exit(SIGINT).code(), ExitCode::from(130));
    }
}
