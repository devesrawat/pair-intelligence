use serde::{Deserialize, Serialize};

/// Stable machine-readable error codes (schema v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthenticated, PolicyDenied, ApprovalRequired, ApprovalExpired, ApprovalPayloadChanged,
    BudgetExceeded, BudgetUnknownPrice, ReservationUnresolved,
    ProviderTimeout, ProviderUnavailable, ProviderDisallowed, ClassifierInvalid,
    MemoryNoEvidence, SourceDeleted, ContextOverflow, Conflict, NotFound, InvalidInput, Internal,
}

#[derive(Debug, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct PairError {
    pub code: ErrorCode,
    pub message: String,
}

impl PairError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

pub type Result<T> = std::result::Result<T, PairError>;
