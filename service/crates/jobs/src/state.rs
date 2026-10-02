use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::types::RunState;

pub(crate) fn state_str(s: RunState) -> &'static str {
    match s {
        RunState::Queued => "queued",
        RunState::Running => "running",
        RunState::WaitingApproval => "waiting_approval",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::Interrupted => "interrupted",
    }
}

pub(crate) fn parse_state(s: &str) -> Result<RunState> {
    Ok(match s {
        "queued" => RunState::Queued,
        "running" => RunState::Running,
        "waiting_approval" => RunState::WaitingApproval,
        "succeeded" => RunState::Succeeded,
        "failed" => RunState::Failed,
        "cancelled" => RunState::Cancelled,
        "interrupted" => RunState::Interrupted,
        other => {
            return Err(PairError::new(
                ErrorCode::Internal,
                format!("unknown run state {other}"),
            ))
        }
    })
}
