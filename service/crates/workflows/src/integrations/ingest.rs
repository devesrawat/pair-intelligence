use super::client::{ChangeBatch, ClientError, Provider, SourceChange, SourceClient};
use super::store::{self, AccountState, IntegrationAccount};
use super::MAX_PAGES_PER_SCOPE;
use crate::daily::store::db_err;
use pair_core::error::{ErrorCode, PairError, Result};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestStats {
    pub inserted: u32,
    pub updated: u32,
    pub tombstoned: u32,
    pub unchanged: u32,
}

impl IngestStats {
    fn merge(&mut self, o: IngestStats) {
        self.inserted += o.inserted;
        self.updated += o.updated;
        self.tombstoned += o.tombstoned;
        self.unchanged += o.unchanged;
    }
}

/// Ingest every allowlisted scope of an account. Scopes outside the allowlist are
/// never requested from the client. A revoked token halts ingestion and persists the
/// `revoked` state; a disconnected account is refused outright.
pub async fn ingest_account(
    pool: &PgPool,
    client: &dyn SourceClient,
    account_id: Uuid,
) -> Result<IngestStats> {
    let account = store::get_account(pool, account_id).await?;
    match account.state {
        AccountState::Connected => {}
        AccountState::Revoked => {
            return Err(PairError::new(
                ErrorCode::Unauthenticated,
                "ingestion halted: token revoked, reconnect required",
            ))
        }
        AccountState::Disconnected => {
            return Err(PairError::new(
                ErrorCode::PolicyDenied,
                "ingestion refused: account is disconnected",
            ))
        }
    }
    let mut total = IngestStats::default();
    for scope in &account.allowlist {
        total.merge(ingest_scope(pool, client, &account, scope).await?);
    }
    tracing::info!(account = %account.id, ?total, "ingestion complete");
    Ok(total)
}

async fn ingest_scope(
    pool: &PgPool,
    client: &dyn SourceClient,
    account: &IntegrationAccount,
    scope: &str,
) -> Result<IngestStats> {
    let mut stats = IngestStats::default();
    for _ in 0..MAX_PAGES_PER_SCOPE {
        let cursor = store::get_cursor(pool, account.id, scope).await?;
        let batch = match client
            .list_changes(account.provider, scope, cursor.as_deref())
            .await
        {
            Ok(b) => b,
            Err(ClientError::TokenRevoked) => {
                store::set_state(pool, account.id, AccountState::Revoked).await?;
                tracing::warn!(account = %account.id, "token revoked; ingestion halted");
                return Err(PairError::new(
                    ErrorCode::Unauthenticated,
                    "ingestion halted: token revoked",
                ));
            }
            Err(ClientError::Unavailable(m)) => {
                return Err(PairError::new(ErrorCode::ProviderUnavailable, m))
            }
        };
        let has_more = batch.has_more;
        stats.merge(apply_batch(pool, account, scope, batch).await?);
        if !has_more {
            break;
        }
    }
    Ok(stats)
}

/// Apply a batch and advance the cursor in one transaction, so a crash replays
/// the batch rather than losing or half-applying it.
async fn apply_batch(
    pool: &PgPool,
    account: &IntegrationAccount,
    scope: &str,
    batch: ChangeBatch,
) -> Result<IngestStats> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    let mut stats = IngestStats::default();
    for change in &batch.changes {
        match change {
            SourceChange::Upsert(item) => {
                let o = upsert(
                    &mut tx,
                    account.id,
                    account.provider,
                    scope,
                    &item.external_id,
                    &item.revision,
                    &item.content,
                )
                .await?;
                match o {
                    Outcome::Inserted => stats.inserted += 1,
                    Outcome::Updated => stats.updated += 1,
                    Outcome::Unchanged => stats.unchanged += 1,
                    Outcome::Tombstoned => {}
                }
            }
            SourceChange::Deleted { external_id } => {
                match tombstone(&mut tx, account.id, external_id).await? {
                    Outcome::Tombstoned => stats.tombstoned += 1,
                    _ => stats.unchanged += 1,
                }
            }
        }
    }
    sqlx::query(
        "INSERT INTO integration_cursors (account_id, scope, cursor) VALUES ($1, $2, $3)
         ON CONFLICT (account_id, scope) DO UPDATE SET cursor = EXCLUDED.cursor",
    )
    .bind(account.id)
    .bind(scope)
    .bind(&batch.next_cursor)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(stats)
}

enum Outcome {
    Inserted,
    Updated,
    Unchanged,
    Tombstoned,
}

async fn upsert(
    tx: &mut Transaction<'_, Postgres>,
    account: Uuid,
    provider: Provider,
    scope: &str,
    external_id: &str,
    revision: &str,
    content: &serde_json::Value,
) -> Result<Outcome> {
    let existing = sqlx::query("SELECT revision FROM integration_sources WHERE account_id = $1 AND external_id = $2 FOR UPDATE")
        .bind(account)
        .bind(external_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db_err)?;
    let Some(row) = existing else {
        sqlx::query(
            "INSERT INTO integration_sources (account_id, external_id, scope, kind, revision, content) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(account)
        .bind(external_id)
        .bind(scope)
        .bind(provider.source_kind().as_str())
        .bind(revision)
        .bind(content)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
        return Ok(Outcome::Inserted);
    };
    let current: String = row.try_get("revision").map_err(db_err)?;
    if current == revision {
        return Ok(Outcome::Unchanged);
    }
    sqlx::query(
        "UPDATE integration_sources SET revision = $3, content = $4, state = 'active', updated_at = now()
         WHERE account_id = $1 AND external_id = $2",
    )
    .bind(account)
    .bind(external_id)
    .bind(revision)
    .bind(content)
    .execute(&mut **tx)
    .await
    .map_err(db_err)?;
    Ok(Outcome::Updated)
}

async fn tombstone(
    tx: &mut Transaction<'_, Postgres>,
    account: Uuid,
    external_id: &str,
) -> Result<Outcome> {
    let done = sqlx::query(
        "UPDATE integration_sources SET state = 'tombstoned', content = NULL, updated_at = now()
         WHERE account_id = $1 AND external_id = $2 AND state = 'active'",
    )
    .bind(account)
    .bind(external_id)
    .execute(&mut **tx)
    .await
    .map_err(db_err)?;
    Ok(if done.rows_affected() > 0 {
        Outcome::Tombstoned
    } else {
        Outcome::Unchanged
    })
}
