use crate::config::APPROVAL_TTL;
use crate::error::db_err;
use crate::hash::is_valid_hash;
use crate::store::plus;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, RunId, TraceId};
use pair_core::traits::Approvals;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

const COMPLETED_STATUS: &str = "completed";

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
        return Err(PairError::new(
            ErrorCode::ApprovalPayloadChanged,
            "payload hash differs from approved hash",
        ));
    }
    Ok(())
}

fn check_live(row: &ApprovalRow) -> Result<()> {
    if row.expires_at <= Utc::now() {
        return Err(PairError::new(
            ErrorCode::ApprovalExpired,
            "approval expired",
        ));
    }
    if row.consumed {
        return Err(PairError::new(
            ErrorCode::Conflict,
            "approval already consumed",
        ));
    }
    Ok(())
}

/// Used by `JobStore::grant`: approval must match the pending hash and be live (not consumed).
pub(crate) async fn validate_grant(
    tx: &mut Transaction<'_, Postgres>,
    id: ApprovalId,
    pending: &str,
) -> Result<()> {
    let row = lock_approval(tx, id).await?;
    check_hash(&row, pending)?;
    check_live(&row)
}

/// Plain single-use consume for callers outside a run. Rejections never consume.
pub(crate) async fn consume_inner(pool: &PgPool, id: ApprovalId, action_hash: &str) -> Result<()> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    let row = lock_approval(&mut tx, id).await?;
    check_hash(&row, action_hash)?;
    check_live(&row)?;
    sqlx::query("UPDATE approvals SET consumed_at = now(), consumed_by = NULL WHERE id = $1")
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    tracing::info!(approval_id = %id, "approval consumed");
    Ok(())
}

/// Authorize exactly one effect intent `(run, key)` with approval `id`, inside the caller's
/// transaction (which has already passed the lease guard, so lock order is run -> approval).
///
/// The approval must hash-match the payload, is bound to one intent via
/// `effect_intents.approval_id` (UNIQUE) and can be re-entered only for that same intent, e.g.
/// after a crash or an ambiguous failure. Re-entry for an intent that is not yet completed
/// re-checks expiry; a completed intent is just replayed from storage. Rejections never consume.
pub(crate) async fn authorize_effect(
    tx: &mut Transaction<'_, Postgres>,
    id: ApprovalId,
    action_hash: &str,
    run: RunId,
    key: &str,
) -> Result<()> {
    let row = lock_approval(tx, id).await?;
    check_hash(&row, action_hash)?;
    let existing: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT status, approval_id FROM effect_intents WHERE run_id = $1 AND effect_key = $2 FOR UPDATE",
    )
    .bind(run.0)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_err)?;
    let bound_elsewhere: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM effect_intents WHERE approval_id = $1 \
           AND NOT (run_id = $2 AND effect_key = $3))",
    )
    .bind(id.0)
    .bind(run.0)
    .bind(key)
    .fetch_one(&mut **tx)
    .await
    .map_err(db_err)?;
    if bound_elsewhere {
        return Err(PairError::new(
            ErrorCode::Conflict,
            "approval already authorizes a different effect",
        ));
    }
    match existing {
        Some((status, Some(bound))) if bound == id.0 => {
            if status != COMPLETED_STATUS && row.expires_at <= Utc::now() {
                return Err(expired());
            }
            return Ok(());
        }
        Some((_, Some(_))) => {
            return Err(PairError::new(
                ErrorCode::Conflict,
                "effect is bound to a different approval",
            ));
        }
        _ => {}
    }
    if row.consumed && row.consumed_by != Some(run.0) {
        return Err(PairError::new(
            ErrorCode::Conflict,
            "approval already consumed",
        ));
    }
    if row.expires_at <= Utc::now() {
        return Err(expired());
    }
    if !row.consumed {
        sqlx::query("UPDATE approvals SET consumed_at = now(), consumed_by = $2 WHERE id = $1")
            .bind(id.0)
            .bind(run.0)
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
        tracing::info!(approval_id = %id, run_id = %run, effect = key, "approval consumed");
    }
    Ok(())
}

fn expired() -> PairError {
    PairError::new(ErrorCode::ApprovalExpired, "approval expired")
}

impl PgApprovals {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Like `approve`, recording the correlation id of the request the approval was granted for.
    pub async fn approve_with_trace(
        &self,
        action_hash: &str,
        actor: &str,
        expiry: DateTime<Utc>,
        trace: TraceId,
    ) -> Result<ApprovalId> {
        self.insert_approval(action_hash, actor, expiry, Some(trace))
            .await
    }

    async fn insert_approval(
        &self,
        action_hash: &str,
        actor: &str,
        expiry: DateTime<Utc>,
        trace: Option<TraceId>,
    ) -> Result<ApprovalId> {
        if !is_valid_hash(action_hash) {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "action hash must be 64 hex chars (sha256)",
            ));
        }
        if actor.trim().is_empty() {
            return Err(PairError::new(ErrorCode::InvalidInput, "actor required"));
        }
        let now = Utc::now();
        if expiry <= now || expiry > plus(now, APPROVAL_TTL)? {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "expiry must be within the next 24 hours",
            ));
        }
        let id = ApprovalId::new();
        // Without a trace the legacy column list is used, so schemas that predate migration 091
        // (single-migration test fixtures, a rolled-back database) keep working.
        let result = match trace {
            Some(t) => {
                sqlx::query(
                    "INSERT INTO approvals (id, action_hash, actor, expires_at, trace_id) \
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(id.0)
                .bind(action_hash)
                .bind(actor)
                .bind(expiry)
                .bind(t.0)
                .execute(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "INSERT INTO approvals (id, action_hash, actor, expires_at) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(id.0)
                .bind(action_hash)
                .bind(actor)
                .bind(expiry)
                .execute(&self.pool)
                .await
            }
        };
        result.map_err(db_err)?;
        tracing::info!(approval_id = %id, actor, trace_id = ?trace, "approval granted");
        Ok(id)
    }

    /// Approve with the default 24 h expiry.
    pub async fn approve_default(&self, action_hash: &str, actor: &str) -> Result<ApprovalId> {
        self.approve(action_hash, actor, plus(Utc::now(), APPROVAL_TTL)?)
            .await
    }
}

#[async_trait]
impl Approvals for PgApprovals {
    /// `expiry` must be in the future and at most 24 h away.
    async fn approve(
        &self,
        action_hash: &str,
        actor: &str,
        expiry: DateTime<Utc>,
    ) -> Result<ApprovalId> {
        self.insert_approval(action_hash, actor, expiry, None).await
    }

    async fn consume(&self, id: ApprovalId, action_hash: &str) -> Result<()> {
        consume_inner(&self.pool, id, action_hash).await
    }
}
