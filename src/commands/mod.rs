pub mod list;
pub mod run;
pub mod topo;

mod filtering;

use std::process::ExitCode;

/// Exit code for a request that cannot be satisfied (usage error).
pub const USAGE_ERROR: u8 = 64;

/// Exit code signaling a transient failure worth retrying later.
pub const TEMPORARY_FAILURE: u8 = 75;

/// Command outcome.
pub enum Exit {
    /// The command succeeded.
    Ok,
    /// The command finished with a specific exit code.
    Code(ExitCode),
}

impl Exit {
    pub const fn code(self) -> ExitCode {
        match self {
            Self::Ok => ExitCode::SUCCESS,
            Self::Code(code) => code,
        }
    }
}
