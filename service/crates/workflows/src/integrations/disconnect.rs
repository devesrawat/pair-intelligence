use super::store::{self, AccountState, IntegrationAccount};
use crate::daily::store::db_err;
use pair_core::error::Result;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionChoice {
    /// Keep sources already derived from the account.
    KeepDerivedSources,
    /// Tombstone every derived source (content removed).
    DeleteDerivedSources,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisconnectReport {
    pub account_id: Uuid,
    pub derived_source_count: i64,
    pub choices: Vec<DeletionChoice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportedSource {
    pub external_id: String,
    pub scope: String,
    pub revision: String,
    pub state: String,
    pub content: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportBundle {
    pub account: IntegrationAccount,
    pub sources: Vec<ExportedSource>,
}

/// Stop all future ingestion and report what exists, with the deletion choices on offer.
/// Cursors are dropped so a later reconnect starts from a clean initial sync.
pub async fn disconnect(pool: &PgPool, account_id: Uuid) -> Result<DisconnectReport> {
    store::set_state(pool, account_id, AccountState::Disconnected).await?;
    sqlx::query("DELETE FROM integration_cursors WHERE account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .map_err(db_err)?;
    let derived_source_count = store::active_source_count(pool, account_id).await?;
    tracing::info!(account = %account_id, derived_source_count, "integration disconnected");
    Ok(DisconnectReport {
        account_id,
        derived_source_count,
        choices: vec![DeletionChoice::KeepDerivedSources, DeletionChoice::DeleteDerivedSources],
    })
}

/// Apply the owner's choice; returns the number of sources tombstoned.
pub async fn apply_deletion_choice(pool: &PgPool, account_id: Uuid, choice: DeletionChoice) -> Result<u64> {
    match choice {
        DeletionChoice::KeepDerivedSources => Ok(0),
        DeletionChoice::DeleteDerivedSources => {
            let done = sqlx::query(
                "UPDATE integration_sources SET state = 'tombstoned', content = NULL, updated_at = now()
                 WHERE account_id = $1 AND state = 'active'",
            )
            .bind(account_id)
            .execute(pool)
            .await
            .map_err(db_err)?;
            Ok(done.rows_affected())
        }
    }
}

/// Portable export of the account's sources (tombstones included, without content).
pub async fn export(pool: &PgPool, account_id: Uuid) -> Result<ExportBundle> {
    let account = store::get_account(pool, account_id).await?;
    let rows = sqlx::query(
        "SELECT external_id, scope, revision, state, content FROM integration_sources
         WHERE account_id = $1 ORDER BY external_id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    let sources = rows
        .iter()
        .map(|r| {
            Ok(ExportedSource {
                external_id: r.try_get("external_id").map_err(db_err)?,
                scope: r.try_get("scope").map_err(db_err)?,
                revision: r.try_get("revision").map_err(db_err)?,
                state: r.try_get("state").map_err(db_err)?,
                content: r.try_get("content").map_err(db_err)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ExportBundle { account, sources })
}
