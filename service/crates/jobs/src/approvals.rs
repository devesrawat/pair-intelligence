use crate::config::APPROVAL_TTL;
use crate::error::db_err;
use crate::hash::is_valid_hash;
use crate::store::plus;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, RunId};
use pair_core::traits::Approvals;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

/// Hash-bound, expiring, single-use approvals.
#[derive(Clone)]
pub struct PgApprovals {
    pool: PgPool,
}

struct ApprovalRow {
    action_hash: String,
    expires_at: DateTime<Utc>,
    consumed_by: Option<Uuid>,
    consumed: bool,
}

async fn lock_approval(tx: &mut Transaction<'_, Postgres>, id: ApprovalId) -> Result<ApprovalRow> {
    let row = sqlx::query("SELECT action_hash, expires_at, consumed_at, consumed_by FROM approvals WHERE id = $1 FOR UPDATE")
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| PairError::new(ErrorCode::NotFound, format!("approval {id}")))?;
    let consumed_at: Option<DateTime<Utc>> = row.try_get("consumed_at").map_err(db_err)?;
    Ok(ApprovalRow {
        action_hash: row.try_get("action_hash").map_err(db_err)?,
        expires_at: row.try_get("expires_at").map_err(db_err)?,
        consumed_by: row.try_get("consumed_by").map_err(db_err)?,
        consumed: consumed_at.is_some(),
    })
}

fn check_hash(row: &ApprovalRow, action_hash: &str) -> Result<()> {
    if row.action_hash != action_hash {
        return Err(PairError::new(ErrorCode::ApprovalPayloadChanged, "payload hash differs from approved hash"));
    }
    Ok(())
}

fn check_live(row: &ApprovalRow) -> Result<()> {
    if row.expires_at <= Utc::now() {
        return Err(PairError::new(ErrorCode::ApprovalExpired, "approval expired"));
    }
    if row.consumed {
        return Err(PairError::new(ErrorCode::Conflict, "approval already consumed"));
    }
    Ok(())
}

/// Used by `JobStore::grant`: approval must match the pending hash and be live (not consumed).
pub(crate) async fn validate_grant(tx: &mut Transaction<'_, Postgres>, id: ApprovalId, pending: &str) -> Result<()> {
    let row = lock_approval(tx, id).await?;
    check_hash(&row, pending)?;
    check_live(&row)
}

/// Check and consume. With `run`, consumption is re-entrant for the same run so a run resumed
/// after a crash between consume and effect completion is not stuck; effects stay exactly-once via
/// their intents. Any other caller sees single-use semantics. Rejections never consume.
pub(crate) async fn consume_inner(pool: &PgPool, id: ApprovalId, action_hash: &str, run: Option<RunId>) -> Result<()> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    let row = lock_approval(&mut tx, id).await?;
    check_hash(&row, action_hash)?;
    if row.consumed && run.is_some() && row.consumed_by == run.map(|r| r.0) {
        return Ok(());
    }
    check_live(&row)?;
    sqlx::query("UPDATE approvals SET consumed_at = now(), consumed_by = $2 WHERE id = $1")
        .bind(id.0)
        .bind(run.map(|r| r.0))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    tracing::info!(approval_id = %id, "approval consumed");
    Ok(())
}

impl PgApprovals {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Approve with the default 24 h expiry.
    pub async fn approve_default(&self, action_hash: &str, actor: &str) -> Result<ApprovalId> {
        self.approve(action_hash, actor, plus(Utc::now(), APPROVAL_TTL)?).await
    }
}

#[async_trait]
impl Approvals for PgApprovals {
    /// `expiry` must be in the future and at most 24 h away.
    async fn approve(&self, action_hash: &str, actor: &str, expiry: DateTime<Utc>) -> Result<ApprovalId> {
        if !is_valid_hash(action_hash) {
            return Err(PairError::new(ErrorCode::InvalidInput, "action hash must be 64 hex chars (sha256)"));
        }
        if actor.trim().is_empty() {
            return Err(PairError::new(ErrorCode::InvalidInput, "actor required"));
        }
        let now = Utc::now();
        if expiry <= now || expiry > plus(now, APPROVAL_TTL)? {
            return Err(PairError::new(ErrorCode::InvalidInput, "expiry must be within the next 24 hours"));
        }
        let id = ApprovalId::new();
        sqlx::query("INSERT INTO approvals (id, action_hash, actor, expires_at) VALUES ($1, $2, $3, $4)")
            .bind(id.0)
            .bind(action_hash)
            .bind(actor)
            .bind(expiry)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        tracing::info!(approval_id = %id, actor, "approval granted");
        Ok(id)
    }

    async fn consume(&self, id: ApprovalId, action_hash: &str) -> Result<()> {
        consume_inner(&self.pool, id, action_hash, None).await
    }
}
