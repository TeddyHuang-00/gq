//! End-to-end tests for `gq run` against a stub `nvidia-smi`.
//!
//! Every test owns a private temporary directory and stub, so tests never touch
//! a real GPU or each other.

use std::{
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

/// Idle card suffix: 5 MiB used, below the default 100 MiB threshold.
const IDLE: &str = ", 49140 MiB, 48536 MiB, 5 MiB, 0 %";
/// Busy card suffix: 1024 MiB used, above the default threshold.
const BUSY: &str = ", 49140 MiB, 48116 MiB, 1024 MiB, 0 %";

/// A per-test temporary directory that removes itself on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path =
            std::env::temp_dir().join(format!("gq-cli-{}-{nanos}-{label}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes an executable `nvidia-smi` stub reporting `cards` and `topo` rows.
fn write_stub(dir: &TempDir, cards: &[(usize, bool)], topo: &[&str]) -> Result<PathBuf> {
    let usage = cards
        .iter()
        .map(|&(index, busy)| format!("{index}{}", if busy { BUSY } else { IDLE }))
        .collect::<Vec<_>>();
    let mut script = String::from("#!/bin/sh\ncase \" $* \" in\n  *\" topo \"*)\n");
    script.push_str(&printf_lines(topo.iter().copied()));
    script.push_str("    ;;\n  *)\n");
    script.push_str(&printf_lines(usage.iter().map(String::as_str)));
    script.push_str("    ;;\nesac\n");
    let path = dir.path().join("nvidia-smi");
    fs::write(&path, script)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Writes a stub that reports `cards` but fails `topo -m`.
fn write_failing_topo_stub(dir: &TempDir, cards: &[(usize, bool)]) -> Result<PathBuf> {
    let usage = cards
        .iter()
        .map(|&(index, busy)| format!("{index}{}", if busy { BUSY } else { IDLE }))
        .collect::<Vec<_>>();
    let mut script = String::from(
        "#!/bin/sh\ncase \" $* \" in\n  *\" topo \"*)\n    echo 'topo failed' >&2\n    exit 1\n    ;;\n  *)\n",
    );
    script.push_str(&printf_lines(usage.iter().map(String::as_str)));
    script.push_str("    ;;\nesac\n");
    let path = dir.path().join("nvidia-smi");
    fs::write(&path, script)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Writes a stub whose usage answer is delayed, to exercise probe bounds.
fn write_slow_stub(dir: &TempDir, seconds: u64) -> Result<PathBuf> {
    let script = format!("#!/bin/sh\nsleep {seconds}\nprintf '%s\\n' '0{IDLE}'\n");
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

/// Builds a `gq` invocation wired to this test's stub.
fn gq(stub: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gq"));
    command
        .env("GQ_NVIDIA_SMI", stub)
        .env_remove("RUST_LOG")
        .env_remove("NO_COLOR");
    command
}

fn run(stub: &Path, args: &[&str]) -> Result<Output> {
    gq(stub).args(args).output().context("cannot run gq")
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .unwrap_or_else(|| panic!("gq was killed by a signal: {:?}", output.status))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn wait_until(mut condition: impl FnMut() -> bool, seconds: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if condition() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("condition not met within {seconds}s")
}

fn wait_for_exit(child: &mut Child, seconds: u64) -> Result<ExitStatus> {
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

fn wait_for_pid(path: &Path, seconds: u64) -> Result<u32> {
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

fn send_sigterm(child: &Child) -> Result<()> {
    let pid = i32::try_from(child.id())?;
    // SAFETY: kill(2) takes integers only; `pid` names the child we spawned.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        bail!(
            "cannot send SIGTERM to {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[test]
fn dry_run_selects_without_executing() -> Result<()> {
    let dir = TempDir::new("dry-run")?;
    let stub = write_stub(&dir, &[(0, false)], &[])?;
    let sentinel = dir.path().join("sentinel");
    let output = gq(&stub)
        .args(["run", "--dry-run", "--", "sh", "-c", r#"touch "$1""#, "sh"])
        .arg(&sentinel)
        .output()?;

    assert_eq!(code(&output), 0, "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("would run on GPUs 0"),
        "stdout: {}",
        text(&output.stdout)
    );
    assert!(!sentinel.exists(), "command ran during dry run");
    Ok(())
}

#[test]
fn executes_with_reserved_env_and_propagates_exit() -> Result<()> {
    let dir = TempDir::new("exec")?;
    let stub = write_stub(&dir, &[(0, false)], &[])?;
    let output = gq(&stub)
        .args([
            "run",
            "--no-wait",
            "-e",
            "FOO=bar",
            "--",
            "sh",
            "-c",
            r#"printf "%s|%s|%s" "$CUDA_VISIBLE_DEVICES" "$GQ_CARDS" "$FOO"; exit 42"#,
        ])
        .output()?;

    assert_eq!(code(&output), 42, "stderr: {}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "0|0|bar");
    Ok(())
}

#[test]
fn busy_gpu_with_no_wait_exits_75() -> Result<()> {
    let dir = TempDir::new("busy")?;
    let stub = write_stub(&dir, &[(0, true)], &[])?;
    let output = run(&stub, &["run", "--no-wait", "--", "true"])?;

    assert_eq!(code(&output), 75, "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("no matching GPUs available after one attempt"),
        "stderr: {}",
        text(&output.stderr)
    );
    Ok(())
}

#[test]
fn timeout_exits_75() -> Result<()> {
    let dir = TempDir::new("timeout")?;
    let stub = write_stub(&dir, &[(0, true)], &[])?;
    let output = run(
        &stub,
        &["run", "--poll", "1", "--timeout", "1", "--", "true"],
    )?;

    assert_eq!(code(&output), 75, "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("timed out"),
        "stderr: {}",
        text(&output.stderr)
    );
    Ok(())
}

#[test]
fn probe_timeout_is_honored() -> Result<()> {
    let dir = TempDir::new("probe-timeout")?;
    let stub = write_slow_stub(&dir, 8)?;
    let sentinel = dir.path().join("sentinel");
    let started = Instant::now();
    let output = gq(&stub)
        .args([
            "run",
            "--poll",
            "1",
            "--timeout",
            "1",
            "--",
            "sh",
            "-c",
            r#"touch "$1""#,
            "sh",
        ])
        .arg(&sentinel)
        .output()?;
    let elapsed = started.elapsed();

    assert_eq!(code(&output), 75, "stderr: {}", text(&output.stderr));
    assert!(!sentinel.exists(), "job ran after the probe deadline");
    assert!(
        elapsed < Duration::from_secs(3),
        "probe was not bounded: {elapsed:?}"
    );
    Ok(())
}

#[test]
fn topology_mismatch_is_an_error_not_a_panic() -> Result<()> {
    let dir = TempDir::new("topo-mismatch")?;
    let topo = ["GPU0 GPU1", "GPU0 X NV8", "GPU1 NV8 X"];
    let stub = write_stub(
        &dir,
        &[(0, false), (1, false), (2, false), (3, false)],
        &topo,
    )?;
    let output = run(
        &stub,
        &[
            "run",
            "--no-wait",
            "-k",
            "2",
            "--strategy",
            "topology-aware",
            "--",
            "true",
        ],
    )?;
    let stderr = text(&output.stderr);

    assert_eq!(code(&output), 1, "stderr: {stderr}");
    assert!(stderr.contains("topology matrix"), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    Ok(())
}

#[test]
fn topology_aware_waits_for_max_score_group() -> Result<()> {
    let dir = TempDir::new("topology")?;
    let topo = [
        "GPU0 GPU1 GPU2 GPU3",
        "GPU0 X NV8 SYS SYS",
        "GPU1 NV8 X SYS SYS",
        "GPU2 SYS SYS X SYS",
        "GPU3 SYS SYS SYS X",
    ];
    let stub = write_stub(
        &dir,
        &[(0, true), (1, false), (2, false), (3, false)],
        &topo,
    )?;
    let output = run(
        &stub,
        &[
            "run",
            "--no-wait",
            "-k",
            "2",
            "--strategy",
            "topology-aware",
            "--",
            "true",
        ],
    )?;
    assert_eq!(code(&output), 75, "stderr: {}", text(&output.stderr));

    // With every card idle the only max-min pair is 0,1.
    let stub = write_stub(
        &dir,
        &[(0, false), (1, false), (2, false), (3, false)],
        &topo,
    )?;
    let output = run(
        &stub,
        &[
            "run",
            "--dry-run",
            "-k",
            "2",
            "--strategy",
            "topology-aware",
            "--",
            "true",
        ],
    )?;
    assert_eq!(code(&output), 0, "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("would run on GPUs 0,1"),
        "stdout: {}",
        text(&output.stdout)
    );
    Ok(())
}

#[test]
fn topology_aware_one_card_warns_and_ignores_topology() -> Result<()> {
    let dir = TempDir::new("topo-one")?;
    let stub = write_failing_topo_stub(&dir, &[(0, false), (1, false)])?;
    let output = run(
        &stub,
        &[
            "run",
            "--dry-run",
            "-k",
            "1",
            "--strategy",
            "topology-aware",
            "--",
            "true",
        ],
    )?;
    let stderr = text(&output.stderr);

    assert_eq!(code(&output), 0, "stderr: {stderr}");
    assert!(
        text(&output.stdout).contains("would run on GPUs 0"),
        "stdout: {}",
        text(&output.stdout)
    );
    assert!(
        stderr.contains(
            "`--strategy topology-aware` has no effect with `-k 1`; selecting the first idle GPU"
        ),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("cannot read GPU topology"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn invalid_requests_exit_64_before_probing() -> Result<()> {
    let dir = TempDir::new("invalid")?;
    // A nonexistent stub proves validation happens before any probe.
    let missing_stub = dir.path().join("nvidia-smi-missing");

    for args in [
        vec!["run"],
        vec!["run", "--env", "CUDA_VISIBLE_DEVICES=0", "--", "true"],
        vec!["run", "--env", "GQ_CARDS=0", "--", "true"],
        vec!["run", "--poll", "0", "--", "true"],
        vec!["run", "--poll", "86401", "--", "true"],
    ] {
        let output = gq(&missing_stub).args(&args).output()?;
        assert_eq!(code(&output), 64, "args: {args:?}");
        assert!(!output.stderr.is_empty(), "args: {args:?}");
    }

    // An empty program name is a usage error, not "command not found".
    let output = gq(&missing_stub).args(["run", "--", ""]).output()?;
    assert_eq!(code(&output), 64, "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("program name must not be empty"),
        "stderr: {}",
        text(&output.stderr)
    );

    let missing_dir = dir.path().join("missing");
    let output = gq(&missing_stub)
        .arg("run")
        .arg("--dir")
        .arg(&missing_dir)
        .args(["--", "true"])
        .output()?;
    assert_eq!(code(&output), 64, "stderr: {}", text(&output.stderr));
    Ok(())
}

#[test]
fn sigterm_forwards_to_child() -> Result<()> {
    let dir = TempDir::new("sigterm")?;
    let stub = write_stub(&dir, &[(0, false)], &[])?;
    let pidfile = dir.path().join("child.pid");
    let mut child = gq(&stub)
        .args([
            "run",
            "--no-wait",
            "--",
            "sh",
            "-c",
            r#"echo $$ > "$1"; sleep 30"#,
            "sh",
        ])
        .arg(&pidfile)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot spawn gq")?;

    let child_pid = match wait_for_pid(&pidfile, 5) {
        Ok(pid) => pid,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{error:#}");
        }
    };
    send_sigterm(&child)?;
    let status = wait_for_exit(&mut child, 5)?;
    let mut diagnostic = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut diagnostic);
    }

    assert_eq!(status.code(), Some(143), "stderr: {diagnostic}");
    let proc = PathBuf::from(format!("/proc/{child_pid}"));
    if let Err(error) = wait_until(|| !proc.exists(), 5) {
        if let Ok(pid) = i32::try_from(child_pid) {
            // SAFETY: kill(2) takes integers only; best-effort cleanup of the
            // leftover child before failing the test.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        return Err(error).with_context(|| format!("child {child_pid} survived SIGTERM"));
    }
    Ok(())
}
