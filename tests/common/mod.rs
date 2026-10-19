//! Shared harness for the `gq run` end-to-end tests.
//!
//! Every test owns a private temporary directory and stub, so tests never touch
//! a real GPU or each other.

#![allow(dead_code)] // each test crate uses only part of the harness

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, kill_process};

/// Idle card memory columns: 5 MiB used, below the default 100 MiB threshold.
const IDLE: &str = ", 49140 MiB, 48536 MiB, 5 MiB, 0 %";
/// Busy card memory columns: 1024 MiB used, above the default threshold.
const BUSY: &str = ", 49140 MiB, 48116 MiB, 1024 MiB, 0 %";

/// UUID the stub reports for card `index`.
pub fn gpu_uuid(index: usize) -> String {
    format!("GPU-{index:08x}-0000-0000-0000-000000000000")
}

/// One usage row for card `index`, with the UUID column the probe expects.
fn usage_row(index: usize, busy: bool) -> String {
    format!(
        "{index}, {}{}",
        gpu_uuid(index),
        if busy { BUSY } else { IDLE }
    )
}

/// A per-test temporary directory that removes itself on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path =
            std::env::temp_dir().join(format!("gq-cli-{}-{nanos}-{label}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes an executable `nvidia-smi` stub reporting `cards` and `topo` rows.
pub fn write_stub(dir: &TempDir, cards: &[(usize, bool)], topo: &[&str]) -> Result<PathBuf> {
    write_app_stub(dir, cards, topo, "")
}

/// Like [`write_stub`], but the compute-apps query prints `apps` and exits 0.
///
/// `apps` is shell code, so it can refer to `$PPID` to report an app owned by
/// the running `gq` process.
pub fn write_app_stub(
    dir: &TempDir,
    cards: &[(usize, bool)],
    topo: &[&str],
    apps: &str,
) -> Result<PathBuf> {
    let usage = cards
        .iter()
        .map(|&(index, busy)| usage_row(index, busy))
        .collect::<Vec<_>>();
    let mut script = String::from("#!/bin/sh\ncase \" $* \" in\n  *\" topo \"*)\n");
    script.push_str(&printf_lines(topo.iter().copied()));
    script.push_str("    ;;\n  *\"--query-compute-apps\"*)\n");
    script.push_str(apps);
    script.push_str("    ;;\n  *)\n");
    script.push_str(&printf_lines(usage.iter().map(String::as_str)));
    script.push_str("    ;;\nesac\n");
    let path = dir.path().join("nvidia-smi");
    fs::write(&path, script)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Writes a stub that reports `cards` but fails `topo -m`.
pub fn write_failing_topo_stub(dir: &TempDir, cards: &[(usize, bool)]) -> Result<PathBuf> {
    let usage = cards
        .iter()
        .map(|&(index, busy)| usage_row(index, busy))
        .collect::<Vec<_>>();
    let mut script = String::from(
        "#!/bin/sh\ncase \" $* \" in\n  *\" topo \"*)\n    echo 'topo failed' >&2\n    exit 1\n    ;;\n  *\"--query-compute-apps\"*)\n    ;;\n  *)\n",
    );
    script.push_str(&printf_lines(usage.iter().map(String::as_str)));
    script.push_str("    ;;\nesac\n");
    let path = dir.path().join("nvidia-smi");
    fs::write(&path, script)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Writes a stub whose usage answer is delayed, to exercise probe bounds.
pub fn write_slow_stub(dir: &TempDir, seconds: u64) -> Result<PathBuf> {
    let script = format!(
        "#!/bin/sh\nsleep {seconds}\nprintf '%s\\n' '{}'\n",
        usage_row(0, false)
    );
    let path = dir.path().join("nvidia-smi");
    fs::write(&path, script)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

fn printf_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    use std::fmt::Write as _;

    let mut script = String::from("    printf '%s\\n'");
    for line in lines {
        let _ = write!(script, " '{line}'");
    }
    script.push('\n');
    script
}

/// Returns the per-test config root, next to the stub.
pub fn config_home(stub: &Path) -> PathBuf {
    stub.parent().map_or_else(
        || std::env::temp_dir().join("gq-test-config"),
        |parent| parent.join("config"),
    )
}

/// Builds a `gq` invocation wired to this test's stub.
pub fn gq(stub: &Path) -> Command {
    let config_home = config_home(stub);
    fs::create_dir_all(&config_home)
        .unwrap_or_else(|error| panic!("cannot create {}: {error}", config_home.display()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_gq"));
    command
        .env("GQ_NVIDIA_SMI", stub)
        .env("XDG_CONFIG_HOME", &config_home)
        .env_remove("RUST_LOG")
        .env_remove("NO_COLOR");
    command
}

pub fn run(stub: &Path, args: &[&str]) -> Result<Output> {
    gq(stub).args(args).output().context("cannot run gq")
}

pub fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .unwrap_or_else(|| panic!("gq was killed by a signal: {:?}", output.status))
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn wait_until(mut condition: impl FnMut() -> bool, seconds: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if condition() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("condition not met within {seconds}s")
}

pub fn wait_for_exit(child: &mut Child, seconds: u64) -> Result<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("gq did not exit within {seconds}s");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn wait_for_pid(path: &Path, seconds: u64) -> Result<u32> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(pid) = contents.trim().parse()
        {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            bail!("pid file {} never appeared", path.display());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn send_sigterm(child: &Child) -> Result<()> {
    let pid = child.id();
    kill_process(Pid::from_child(child), Signal::TERM)
        .with_context(|| format!("cannot send SIGTERM to {pid}"))
}
