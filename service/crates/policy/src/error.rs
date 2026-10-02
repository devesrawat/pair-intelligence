use pair_core::error::{ErrorCode, PairError};

/// Failure to build a policy engine. Any of these means no engine exists, so nothing is authorized.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy config unreadable: {0}")]
    Io(#[from] std::io::Error),
    #[error("policy config invalid: {0}")]
    Invalid(String),
}

impl From<PolicyError> for PairError {
    fn from(e: PolicyError) -> Self {
        PairError::new(ErrorCode::InvalidInput, e.to_string())
    }
}
