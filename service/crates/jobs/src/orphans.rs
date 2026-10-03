//! Sweeper for effect intents left unresolved on runs that are already terminal (failed by
//! deadline mid-effect, cancelled, ...). No worker will ever resume such a run, so without this an
//! `executing`/`unknown` intent would stay ambiguous forever. A run that fails with unresolved
//! intents is additionally marked `unreconciled_effect` (see `Worker`) until the sweep settles them.
use crate::effects::{Intent, Reconciliation};
use crate::error::db_err;
use crate::store::JobStore;
use async_trait::async_trait;
use pair_core::error::Result;
use pair_core::ids::RunId;
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

/// Failure-text prefix of a run that ended with intents whose effect outcome is unknown.
pub const UNRECONCILED_EFFECT: &str = "unreconciled_effect";
/// Intents are swept in batches of this size per call.
const SWEEP_BATCH: i64 = 100;

/// Asks the outside world whether an orphaned effect happened.
#[async_trait]
pub trait IntentReconciler: Send + Sync {
    async fn reconcile(&self, intent: Intent) -> Result<Reconciliation>;
}

/// Outcome counts of one `reconcile_orphaned_intents` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrphanSweep {
    /// Intents confirmed as happened (now `completed`).
    pub applied: u64,
    /// Intents confirmed as never happened (now `not_applied`).
    pub not_applied: u64,
    /// Intents the reconciler could not decide; they stay unresolved and are retried next sweep.
    pub undecided: u64,
}

impl JobStore {
    /// Reconcile `intended`/`executing`/`unknown` intents of terminal runs. Only runs that ended
    /// more than one lease TTL ago are considered, so a cancelled run's still-draining worker
    /// (which notices the cancel within a heartbeat) cannot race the sweeper.
    pub async fn reconcile_orphaned_intents(
        &self,
        reconciler: &dyn IntentReconciler,
    ) -> Result<OrphanSweep> {
        let grace_ms = i64::try_from(self.cfg.lease_ttl.as_millis()).unwrap_or(i64::MAX);
        let rows = sqlx::query(
            "SELECT i.id, i.run_id, i.effect_key, i.payload FROM effect_intents i \
             JOIN workflow_runs w ON w.id = i.run_id \
             WHERE i.status IN ('intended', 'executing', 'unknown') \
               AND w.state IN ('succeeded', 'failed', 'cancelled') \
               AND w.finished_at <= now() - ($1::double precision * interval '1 millisecond') \
             ORDER BY i.created_at LIMIT $2",
        )
        .bind(grace_ms as f64)
        .bind(SWEEP_BATCH)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut sweep = OrphanSweep::default();
        for row in rows {
            let id: Uuid = row.try_get("id").map_err(db_err)?;
            let intent = Intent {
                run_id: RunId(row.try_get("run_id").map_err(db_err)?),
                key: row.try_get("effect_key").map_err(db_err)?,
                payload: row.try_get("payload").map_err(db_err)?,
            };
            match reconciler.reconcile(intent.clone()).await {
                Ok(Reconciliation::Applied(result)) => {
                    self.settle_orphan(id, "completed", Some(&result)).await?;
                    sweep.applied += 1;
                }
                Ok(Reconciliation::NotApplied) => {
                    self.settle_orphan(id, "not_applied", None).await?;
                    sweep.not_applied += 1;
                }
                Err(e) => {
                    tracing::warn!(run_id = %intent.run_id, effect = %intent.key, error = %e, "orphan reconcile undecided");
                    sweep.undecided += 1;
                }
            }
        }
        Ok(sweep)
    }

    async fn settle_orphan(&self, id: Uuid, status: &str, result: Option<&Value>) -> Result<()> {
        sqlx::query(
            "UPDATE effect_intents SET status = $2, result = $3, completed_at = now() \
             WHERE id = $1 AND status IN ('intended', 'executing', 'unknown')",
        )
        .bind(id)
        .bind(status)
        .bind(result)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }
}
