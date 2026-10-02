use serde::{Deserialize, Serialize};

/// Stable machine-readable error codes (schema v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthenticated,
    PolicyDenied,
    ApprovalRequired,
    ApprovalExpired,
    ApprovalPayloadChanged,
    BudgetExceeded,
    BudgetUnknownPrice,
    ReservationUnresolved,
    ProviderTimeout,
    ProviderUnavailable,
    ProviderDisallowed,
    ClassifierInvalid,
    MemoryNoEvidence,
    SourceDeleted,
    ContextOverflow,
    Conflict,
    NotFound,
    InvalidInput,
    /// A jobs run limit (steps, tool calls, wall time) was reached.
    LimitExceeded,
    /// No candidate model has the capabilities the request requires.
    CapabilityMismatch,
    Internal,
}

#[derive(Debug, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct PairError {
    pub code: ErrorCode,
    pub message: String,
}

impl PairError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, PairError>;

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACTS_JSON: &str = include_str!("../../../../config/contracts.json");

    fn all_codes() -> Vec<ErrorCode> {
        use ErrorCode::*;
        vec![
            Unauthenticated,
            PolicyDenied,
            ApprovalRequired,
            ApprovalExpired,
            ApprovalPayloadChanged,
            BudgetExceeded,
            BudgetUnknownPrice,
            ReservationUnresolved,
            ProviderTimeout,
            ProviderUnavailable,
            ProviderDisallowed,
            ClassifierInvalid,
            MemoryNoEvidence,
            SourceDeleted,
            ContextOverflow,
            Conflict,
            NotFound,
            InvalidInput,
            LimitExceeded,
            CapabilityMismatch,
            Internal,
        ]
    }

    #[test]
    fn test_error_codes_contract_matches_enum() {
        let doc: serde_json::Value = serde_json::from_str(CONTRACTS_JSON).expect("contracts json");
        let mut listed: Vec<String> = doc["error_codes"]
            .as_array()
            .expect("array")
            .iter()
            .map(|v| v.as_str().expect("str").to_owned())
            .collect();
        let mut actual: Vec<String> = all_codes()
            .into_iter()
            .map(|c| {
                serde_json::to_value(c)
                    .expect("ser")
                    .as_str()
                    .expect("str")
                    .to_owned()
            })
            .collect();
        listed.sort();
        actual.sort();
        assert_eq!(listed, actual);
    }
}
