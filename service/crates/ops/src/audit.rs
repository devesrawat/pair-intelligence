//! Audit health: `tool_executions` rows stuck in `started`.
//!
//! The gate writes a `started` row before a tool runs and closes it afterwards. A row that stays
//! `started` means the process died (or the tool never returned) mid-call: the action may or may
//! not have happened. Nothing sweeps these rows (they are evidence), so they are reported here for
//! an operator or a readiness check. Everything in this module is read-only.
use crate::error::{OpsError, Result};
use chrono::Duration;
use sqlx::PgPool;
use uuid::Uuid;

/// Cap on rows returned by [`stuck_executions`]; [`stuck_count`] gives the true total.
pub const MAX_REPORTED: i64 = 500;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StuckExecution {
    pub id: Uuid,
    pub task_id: Uuid,
    pub trace_id: Uuid,
    pub tool: String,
    pub decision: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Seconds since the row was written, as measured by the database clock.
    pub age_secs: i64,
}

fn threshold_secs(older_than: Duration) -> Result<i64> {
    let secs = older_than.num_seconds();
    if secs < 1 {
        return Err(OpsError::InvalidArgument(
            "the stuck-execution threshold must be at least one second".into(),
        ));
    }
    Ok(secs)
}

/// `started` rows older than `older_than`, oldest first, at most [`MAX_REPORTED`].
pub async fn stuck_executions(pool: &PgPool, older_than: Duration) -> Result<Vec<StuckExecution>> {
    let secs = threshold_secs(older_than)?;
    let rows = sqlx::query_as::<_, StuckExecution>(
        "SELECT id, task_id, trace_id, tool, decision, started_at, \
                floor(extract(epoch FROM now() - started_at))::bigint AS age_secs \
         FROM tool_executions \
         WHERE outcome = 'started' \
           AND started_at < now() - make_interval(secs => $1::double precision) \
         ORDER BY started_at, id LIMIT $2",
    )
    .bind(secs)
    .bind(MAX_REPORTED)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Number of `started` rows older than `older_than`: the value a readiness check or alert wants.
pub async fn stuck_count(pool: &PgPool, older_than: Duration) -> Result<i64> {
    let secs = threshold_secs(older_than)?;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_executions \
         WHERE outcome = 'started' AND started_at < now() - make_interval(secs => $1::double precision)",
    )
    .bind(secs)
    .fetch_one(pool)
    .await?;
    Ok(n)
}
