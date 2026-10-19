//! `gq run` selection waiting: busy cards, timeouts, and bounded probes.

mod common;

use std::time::{Duration, Instant};

use anyhow::Result;
use common::{TempDir, code, gq, run, text, write_slow_stub, write_stub};

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
