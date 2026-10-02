//! Environment failure vs implementation defect.
use super::runner::CmdReport;
use serde::{Deserialize, Serialize};

const EXIT_NOT_EXECUTABLE: i32 = 126;
const EXIT_NOT_FOUND: i32 = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Command missing, not executable, or timed out: not evidence about the patch.
    Environment,
    /// The command ran to completion and reported failure.
    ImplementationDefect,
}

/// `None` when the command passed.
pub fn classify_failure(r: &CmdReport) -> Option<FailureClass> {
    if r.passed() {
        return None;
    }
    let env = r.timed_out
        || r.spawn_error.is_some()
        || matches!(r.exit_code, Some(EXIT_NOT_FOUND | EXIT_NOT_EXECUTABLE))
        || r.exit_code.is_none();
    Some(if env {
        FailureClass::Environment
    } else {
        FailureClass::ImplementationDefect
    })
}
