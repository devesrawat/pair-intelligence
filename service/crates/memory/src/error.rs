//! Mapping from database errors to the shared `PairError` contract.
use pair_core::error::{ErrorCode, PairError};

const PG_FOREIGN_KEY_VIOLATION: &str = "23503";
const PG_UNIQUE_VIOLATION: &str = "23505";
const PG_CHECK_VIOLATION: &str = "23514";

/// Convert a sqlx error, logging the full detail and exposing only a stable code.
pub fn db_err(err: sqlx::Error) -> PairError {
    let code = err
        .as_database_error()
        .and_then(|d| d.code().map(|c| c.to_string()));
    tracing::error!(error = %err, pg_code = ?code, "memory store database error");
    match code.as_deref() {
        Some(PG_FOREIGN_KEY_VIOLATION) => {
            PairError::new(ErrorCode::InvalidInput, "referenced record does not exist")
        }
        Some(PG_UNIQUE_VIOLATION) => PairError::new(ErrorCode::Conflict, "record already exists"),
        Some(PG_CHECK_VIOLATION) => PairError::new(
            ErrorCode::InvalidInput,
            "constraint violated (e.g. memory without evidence)",
        ),
        _ => PairError::new(ErrorCode::Internal, "database error"),
    }
}

pub fn not_found(what: &str, id: impl std::fmt::Display) -> PairError {
    PairError::new(ErrorCode::NotFound, format!("{what} {id} not found"))
}

pub fn invalid(msg: impl Into<String>) -> PairError {
    PairError::new(ErrorCode::InvalidInput, msg)
}
