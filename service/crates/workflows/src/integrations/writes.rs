//! External writes (send mail, create/modify events, relabel) are disabled by default.
//! Even when the owner flips the flag, the adapter surface has no write path: the most
//! this module will say is that approval is required.
use super::store;
use pair_core::error::{ErrorCode, PairError, Result};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalWrite {
    SendMessage { to: String, subject: String },
    ModifyLabels { message_id: String },
    CreateEvent { calendar: String, title: String },
    UpdateEvent { event_id: String },
}

pub async fn attempt_write(pool: &PgPool, account_id: Uuid, write: &ExternalWrite) -> Result<()> {
    let account = store::get_account(pool, account_id).await?;
    if !account.writes_enabled {
        tracing::warn!(account = %account_id, ?write, "external write denied: writes disabled");
        return Err(PairError::new(
            ErrorCode::PolicyDenied,
            "external writes are disabled for this account",
        ));
    }
    Err(PairError::new(
        ErrorCode::ApprovalRequired,
        "external writes need an approved, hash-bound action",
    ))
}
