//! Requeue for the periodic sweeper hosted by `pair-api`.
use crate::error::db_err;
use crate::store::JobStore;
use pair_core::error::Result;
use pair_core::ids::RunId;

/// How many times a run may be interrupted before it is failed instead of requeued. The Nth
/// interruption fails the run: a step that kills its worker every time must not loop forever.
pub const MAX_INTERRUPTS: u32 = 5;

/// What one requeue pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequeueSweep {
    /// Interrupted runs made claimable again.
    pub requeued: u64,
    /// Interrupted runs that hit [`MAX_INTERRUPTS`] and were failed.
    pub failed: u64,
}

/// Running time accrued by the claim being ended (same rule as the lease sweeper's).
const ACCRUED_ACTIVE_MS: &str = "GREATEST(0, (EXTRACT(EPOCH FROM \
     (LEAST(now(), COALESCE(lease_expires_at, now())) - COALESCE(claimed_at, now()))) * 1000)::bigint)";

fn failure_text() -> String {
    format!(
        "interrupted {MAX_INTERRUPTS} times (the worker died or its lease lapsed each time); \
         not retried again. Inspect the run, then resume it by hand if it is safe to"
    )
}

impl JobStore {
    /// Make every crash-`interrupted` run claimable again (the step `sweep_expired` leaves open)
    /// and return how many were requeued. A run interrupted [`MAX_INTERRUPTS`] times is failed
    /// instead, and a run an operator interrupted on purpose is left alone. The lease epoch is
    /// untouched: the next claim bumps it, which fences the worker whose lease lapsed.
    pub async fn requeue_interrupted(&self) -> Result<u64> {
        Ok(self.requeue_sweep().await?.requeued)
    }

    /// Like [`Self::requeue_interrupted`], reporting the failed runs as well. One statement, so a
    /// run is either requeued or failed, never both.
    pub async fn requeue_sweep(&self) -> Result<RequeueSweep> {
        let limit = i32::try_from(MAX_INTERRUPTS).unwrap_or(i32::MAX);
        let (requeued, failed): (i64, i64) = sqlx::query_as(
            "WITH due AS ( \
               SELECT id, interrupt_count FROM workflow_runs \
               WHERE state = 'interrupted' AND operator_interrupt_epoch IS DISTINCT FROM lease_epoch \
               FOR UPDATE SKIP LOCKED), \
             failed AS ( \
               UPDATE workflow_runs r SET state = 'failed', failure = $2, finished_at = now(), \
                 updated_at = now(), lease_owner = NULL, lease_expires_at = NULL, \
                 interrupt_count = r.interrupt_count + 1 \
               FROM due WHERE r.id = due.id AND due.interrupt_count + 1 >= $1 RETURNING r.id), \
             requeued AS ( \
               UPDATE workflow_runs r SET state = 'queued', lease_owner = NULL, \
                 lease_expires_at = NULL, updated_at = now(), \
                 interrupt_count = r.interrupt_count + 1 \
               FROM due WHERE r.id = due.id AND due.interrupt_count + 1 < $1 RETURNING r.id) \
             SELECT (SELECT count(*) FROM requeued), (SELECT count(*) FROM failed)",
        )
        .bind(limit)
        .bind(failure_text())
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        let sweep = RequeueSweep {
            requeued: u64::try_from(requeued).unwrap_or(0),
            failed: u64::try_from(failed).unwrap_or(0),
        };
        if sweep.requeued > 0 {
            tracing::warn!(count = sweep.requeued, "interrupted runs requeued");
        }
        if sweep.failed > 0 {
            tracing::error!(
                count = sweep.failed,
                limit = MAX_INTERRUPTS,
                "runs failed after repeated interruptions"
            );
        }
        Ok(sweep)
    }

    /// Interrupt a `running` run on purpose (the operator's stop). The sweeper does not requeue
    /// it; `resume` is the way back. Returns whether the run was running.
    pub async fn operator_interrupt(&self, id: RunId) -> Result<bool> {
        let sql = format!(
            "UPDATE workflow_runs SET state = 'interrupted', lease_owner = NULL, \
             operator_interrupt_epoch = lease_epoch, updated_at = now(), \
             active_base_ms = active_base_ms + {ACCRUED_ACTIVE_MS} \
             WHERE id = $1 AND state = 'running'"
        );
        let res = sqlx::query(&sql)
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(res.rows_affected() == 1)
    }
}
