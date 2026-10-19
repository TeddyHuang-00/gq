//! `gq run` execution: dry runs, env, exit status, and SIGTERM.

mod common;

use std::{io::Read, path::PathBuf, process::Stdio};

use anyhow::{Context, Result, bail};
use common::{
    TempDir, code, gq, run, send_sigterm, text, wait_for_exit, wait_for_pid, wait_until, write_stub,
};

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
    assert_eq!(
        text(&output.stdout),
        "would run on GPUs 0\n",
        "stdout: {}",
        text(&output.stdout)
    );
    assert!(
        text(&output.stderr).contains("add -v"),
        "stderr: {}",
        text(&output.stderr)
    );
    assert!(!sentinel.exists(), "command ran during dry run");

    let verbose = run(&stub, &["-v", "run", "--dry-run", "--", "true"])?;
    assert_eq!(code(&verbose), 0, "stderr: {}", text(&verbose.stderr));
    assert!(
        !text(&verbose.stderr).contains("add -v"),
        "stderr: {}",
        text(&verbose.stderr)
    );
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
        let _ = child.kill();
        return Err(error).with_context(|| format!("child {child_pid} survived SIGTERM"));
    }
    Ok(())
}
