//! `gq` logging: the default level hides info, and `-v` shows it.

mod common;

use anyhow::Result;
use common::{TempDir, code, run, text, write_stub};

#[test]
fn default_log_level_hides_info_and_verbose_shows_it() -> Result<()> {
    let dir = TempDir::new("log-level")?;
    let stub = write_stub(&dir, &[(0, false)], &[])?;

    let default = run(&stub, &["list"])?;
    assert_eq!(code(&default), 0, "stderr: {}", text(&default.stderr));
    assert!(
        !text(&default.stderr).contains("idle rule"),
        "stderr: {}",
        text(&default.stderr)
    );

    let verbose = run(&stub, &["-v", "list"])?;
    assert_eq!(code(&verbose), 0, "stderr: {}", text(&verbose.stderr));
    assert!(
        text(&verbose.stderr).contains("idle rule"),
        "stderr: {}",
        text(&verbose.stderr)
    );
    Ok(())
}
