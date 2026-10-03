//! Claims on a logical turn (conversation + `client_message_id`), taken before any paid work.

use std::time::Duration;

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TaskId};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// How long past the turn budget an `in_progress` claim stays untouchable. The budget bounds the
/// provider attempts only; the classifier and the database come on top, and a process that died
/// mid-turn must not block its retries forever.
pub const CLAIM_LEASE_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// This request owns the turn and may do the paid work.
    Acquired,
    /// An earlier request finished it: return the stored answer.
    Done,
    /// Another request is running it right now.
    InProgress,
    /// The same id was used with different content.
    ContentMismatch,
}

fn db_err(e: &sqlx::Error) -> PairError {
    tracing::error!(error = %e, "turn claim database error");
    PairError::new(ErrorCode::Internal, "turn claim database error")
}

/// Identity of a turn's content: a replay must carry the same message under the same class.
pub fn content_hash(message: &str, class: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(class.as_bytes());
    hasher.update([0u8]);
    hasher.update(message.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[derive(Clone)]
pub struct ClaimStore {
    pool: PgPool,
}

impl ClaimStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Take the claim for `task`, or report why it cannot be taken. One transaction: the insert
    /// either wins outright or the existing row is locked and judged.
    pub async fn claim(
        &self,
        task: TaskId,
        conversation: ConversationId,
        client_message_id: &str,
        content_sha256: &str,
        lease: Duration,
    ) -> Result<Claim> {
        let mut tx = self.pool.begin().await.map_err(|e| db_err(&e))?;
        let inserted = sqlx::query(
            "INSERT INTO turn_claims (task_id, conversation_id, client_message_id, content_sha256, state) \
             VALUES ($1, $2, $3, $4, 'in_progress') ON CONFLICT DO NOTHING",
        )
        .bind(task.0)
        .bind(conversation.0)
        .bind(client_message_id)
        .bind(content_sha256)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_err(&e))?
        .rows_affected();
        let outcome = if inserted == 1 {
            Claim::Acquired
        } else {
            judge_existing(&mut tx, task, content_sha256, lease).await?
        };
        tx.commit().await.map_err(|e| db_err(&e))?;
        Ok(outcome)
    }

    /// Record how the claimed turn ended. A failure to write is logged, not surfaced: the lease
    /// expires on its own and the turn's result is already decided.
    pub async fn finish(&self, task: TaskId, done: bool) {
        let state = if done { "done" } else { "failed" };
        let result = sqlx::query(
            "UPDATE turn_claims SET state = $2, finished_at = now() WHERE task_id = $1",
        )
        .bind(task.0)
        .bind(state)
        .execute(&self.pool)
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, %task, state, "could not record the end of a turn claim");
        }
    }
}

/// Lock the existing claim row and decide: a different content hash is a mismatch, a finished
/// turn is done, a failed (or expired in-progress) one is taken over, anything else is running.
async fn judge_existing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: TaskId,
    content_sha256: &str,
    lease: Duration,
) -> Result<Claim> {
    let (stored_hash, state, stale): (String, String, bool) = sqlx::query_as(
        "SELECT content_sha256, state, claimed_at < now() - make_interval(secs => $2) \
         FROM turn_claims WHERE task_id = $1 FOR UPDATE",
    )
    .bind(task.0)
    .bind(lease.as_secs_f64())
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| db_err(&e))?;
    if stored_hash != content_sha256 {
        return Ok(Claim::ContentMismatch);
    }
    if state == "done" {
        return Ok(Claim::Done);
    }
    if state != "failed" && !stale {
        return Ok(Claim::InProgress);
    }
    sqlx::query(
        "UPDATE turn_claims SET state = 'in_progress', claimed_at = now(), finished_at = NULL \
         WHERE task_id = $1",
    )
    .bind(task.0)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_err(&e))?;
    Ok(Claim::Acquired)
}
