//! `gq run` argument validation: usage errors exit 64 before any probe.

mod common;

use anyhow::Result;
use common::{TempDir, code, gq, text};

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
