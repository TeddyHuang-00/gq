//! `gq run` compute-app accounting: foreign and own apps, and probe failures.

mod common;

use anyhow::Result;
use common::{TempDir, code, gpu_uuid, run, text, write_app_stub};

#[test]
fn foreign_app_marks_idle_card_in_use_until_limit_is_raised() -> Result<()> {
    let dir = TempDir::new("foreign-app")?;
    // The pid does not exist, which must count as foreign.
    let apps = format!("    printf '%s, %s\\n' '{}' '4294967295'\n", gpu_uuid(0));
    let stub = write_app_stub(&dir, &[(0, false)], &[], &apps)?;

    let listed = run(&stub, &["list"])?;
    assert_eq!(code(&listed), 0, "stderr: {}", text(&listed.stderr));
    assert!(
        text(&listed.stdout).contains("In use"),
        "stdout: {}",
        text(&listed.stdout)
    );

    let blocked = run(&stub, &["run", "--no-wait", "--", "true"])?;
    assert_eq!(code(&blocked), 75, "stderr: {}", text(&blocked.stderr));

    let listed = run(&stub, &["list", "--max-foreign-apps", "1"])?;
    assert_eq!(code(&listed), 0, "stderr: {}", text(&listed.stderr));
    assert!(
        text(&listed.stdout).contains("Available"),
        "stdout: {}",
        text(&listed.stdout)
    );

    let allowed = run(
        &stub,
        &["run", "--no-wait", "--max-foreign-apps", "1", "--", "true"],
    )?;
    assert_eq!(code(&allowed), 0, "stderr: {}", text(&allowed.stderr));
    Ok(())
}

#[test]
fn own_app_counts_toward_max_any_apps_but_not_foreign() -> Result<()> {
    let dir = TempDir::new("own-app")?;
    // `$PPID` is the live gq process, so the app belongs to the same user.
    let apps = format!("    printf '%s, %s\\n' '{}' \"$PPID\"\n", gpu_uuid(0));
    let stub = write_app_stub(&dir, &[(0, false)], &[], &apps)?;

    for args in [&["list"][..], &["list", "--max-any-apps", "1"]] {
        let listed = run(&stub, args)?;
        assert_eq!(code(&listed), 0, "args: {args:?}");
        assert!(
            text(&listed.stdout).contains("Available"),
            "args: {args:?}; stdout: {}",
            text(&listed.stdout)
        );
    }

    let listed = run(&stub, &["list", "--max-any-apps", "0"])?;
    assert_eq!(code(&listed), 0, "stderr: {}", text(&listed.stderr));
    assert!(
        text(&listed.stdout).contains("In use"),
        "stdout: {}",
        text(&listed.stdout)
    );
    Ok(())
}

#[test]
fn compute_apps_probe_failure_is_not_treated_as_no_apps() -> Result<()> {
    let dir = TempDir::new("app-probe-failure")?;
    let stub = write_app_stub(&dir, &[(0, false)], &[], "    exit 1\n")?;

    let listed = run(&stub, &["list"])?;
    assert_eq!(code(&listed), 1, "stderr: {}", text(&listed.stderr));

    let ran = run(&stub, &["run", "--no-wait", "--", "true"])?;
    assert_eq!(code(&ran), 75, "stderr: {}", text(&ran.stderr));
    Ok(())
}
