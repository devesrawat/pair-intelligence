use pair_core::error::{ErrorCode, PairError};

/// Message carried by the `Conflict` error returned when a worker no longer owns a run's lease.
pub const LEASE_LOST_MSG: &str = "lease lost";

pub(crate) fn db_err(e: sqlx::Error) -> PairError {
    tracing::error!(error = %e, "jobs database error");
    PairError::new(ErrorCode::Internal, format!("database error: {e}"))
}

pub(crate) fn lease_lost() -> PairError {
    PairError::new(ErrorCode::Conflict, LEASE_LOST_MSG)
}

pub(crate) fn is_lease_lost(e: &PairError) -> bool {
    e.code == ErrorCode::Conflict && e.message == LEASE_LOST_MSG
}

/// Outcome classification for a failed step.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// Safe to retry; the worker retries with capped exponential backoff.
    #[error("transient: {0}")]
    Transient(String),
    /// Not retryable; the run fails.
    #[error("{0}")]
    Fatal(#[from] PairError),
    /// A run limit (tool calls) was hit; the run fails.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
}
