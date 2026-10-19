//! `gq run` topology-aware selection: scores, overrides, target misses.

mod common;

use std::fs;

use anyhow::Result;
use common::{TempDir, code, config_home, run, text, write_failing_topo_stub, write_stub};

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
fn config_file_overrides_topology_scores() -> Result<()> {
    let dir = TempDir::new("config")?;
    let topo = [
        "GPU0 GPU1 GPU2 GPU3",
        "GPU0 X NV8 SYS SYS",
        "GPU1 NV8 X SYS SYS",
        "GPU2 SYS SYS X SYS",
        "GPU3 SYS SYS SYS X",
    ];
    let stub = write_stub(
        &dir,
        &[(0, false), (1, false), (2, false), (3, false)],
        &topo,
    )?;
    let args = [
        "run",
        "--dry-run",
        "-k",
        "2",
        "--strategy",
        "topology-aware",
        "--",
        "true",
    ];

    let output = run(&stub, &args)?;
    assert_eq!(code(&output), 0, "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("would run on GPUs 0,1"),
        "stdout: {}",
        text(&output.stdout)
    );

    let config_dir = config_home(&stub).join("gq");
    fs::create_dir_all(&config_dir)?;
    fs::write(
        config_dir.join("config.toml"),
        "[link.bandwidth]\nsys = 10000\n",
    )?;
    let output = run(&stub, &args)?;
    assert_eq!(code(&output), 0, "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("would run on GPUs 0,2"),
        "stdout: {}",
        text(&output.stdout)
    );
    Ok(())
}

#[test]
fn topology_target_mismatch_is_explained() -> Result<()> {
    let dir = TempDir::new("topo-target")?;
    let topo = [
        "GPU0 GPU1 GPU2 GPU3",
        "GPU0 X NV8 SYS SYS",
        "GPU1 NV8 X SYS SYS",
        "GPU2 SYS SYS X SYS",
        "GPU3 SYS SYS SYS X",
    ];
    // The best pair (0,1) is busy; the idle pair (2,3) does not match the
    // target, so selection fails even though two cards are idle.
    let stub = write_stub(&dir, &[(0, true), (1, true), (2, false), (3, false)], &topo)?;
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

    assert_eq!(code(&output), 75, "stderr: {stderr}");
    assert!(stderr.contains("topology target"), "stderr: {stderr}");
    assert!(stderr.contains("--strategy first"), "stderr: {stderr}");
    assert!(
        !stderr.contains("candidate GPUs idle; need 2"),
        "stderr: {stderr}"
    );

    // The same miss under --dry-run keeps the single stdout line and hints at
    // the stderr logs.
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
    let stderr = text(&output.stderr);
    assert_eq!(code(&output), 75, "stderr: {stderr}");
    assert_eq!(
        text(&output.stdout),
        "no GPUs match the current constraints\n",
        "stdout: {}",
        text(&output.stdout)
    );
    assert!(stderr.contains("add -v"), "stderr: {stderr}");
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
