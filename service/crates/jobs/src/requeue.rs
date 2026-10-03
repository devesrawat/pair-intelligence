//! Requeue for the periodic sweeper hosted by `pair-api`.
use crate::error::db_err;
use crate::store::JobStore;
use pair_core::error::Result;

impl JobStore {
    /// Make every `interrupted` run claimable again (the step `sweep_expired` leaves open) and
    /// return how many were requeued. The lease epoch is untouched: the next claim bumps it, which
    /// fences the worker whose lease lapsed.
    pub async fn requeue_interrupted(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE workflow_runs SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL, \
             updated_at = now() WHERE state = 'interrupted'",
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() > 0 {
            tracing::warn!(count = res.rows_affected(), "interrupted runs requeued");
        }
        Ok(res.rows_affected())
    }
}
