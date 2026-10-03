//! Durable intent records for external side effects (send, push, ...).
//!
//! Protocol: the intent row is written BEFORE the effect, moved to `executing` (a CAS) right
//! before the effect closure is called, and completed after. If the process dies in between, the
//! row stays `intended`/`executing`; on resume it is marked `unknown` and the caller-supplied
//! reconcile callback decides whether the effect happened. The effect is never re-run blindly.
//!
//! Every transition runs in a transaction that first takes the run's lease guard (owner + epoch +
//! `running` + unexpired, held as a row share lock). A worker that lost its lease, or whose run
//! was cancelled, therefore cannot insert, advance, complete or delete any intent: the write
//! fails with "lease lost" instead of racing the new owner.
use crate::approvals::authorize_effect;
use crate::error::{db_err, lease_lost, StepError};
use crate::hash::action_hash;
use crate::step::StepCtx;
use crate::store::JobStore;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, RunId};
use serde_json::Value;
use sqlx::Row;
use std::future::Future;
use uuid::Uuid;

pub(crate) const INTENDED: &str = "intended";
pub(crate) const EXECUTING: &str = "executing";
pub(crate) const UNKNOWN: &str = "unknown";
pub(crate) const COMPLETED: &str = "completed";

/// Failure of the effect closure.
#[derive(Debug)]
pub enum EffectError {
    /// Definitely did not happen; safe to retry.
    Failed(String),
    /// May or may not have happened; must be reconciled before any retry.
    Ambiguous(String),
}

/// Answer from the reconcile callback.
#[derive(Debug)]
pub enum Reconciliation {
    /// The effect happened; carries its result.
    Applied(Value),
    /// The effect did not happen; it is safe to run it.
    NotApplied,
}

/// The intent handed to the reconcile callback.
#[derive(Debug, Clone)]
pub struct Intent {
    pub run_id: RunId,
    pub key: String,
    pub payload: Value,
}

/// Approval bound to an effect: the id attached to the run and the hash of the executed payload.
struct Approval {
    id: ApprovalId,
    hash: String,
}

enum Begun {
    Fresh,
    Existing {
        status: String,
        payload: Value,
        result: Option<Value>,
    },
}

impl StepCtx {
    /// Run `exec` at most once per `(run, key)` across crashes, resumes and lease loss.
    pub async fn effect<E, EF, R, RF>(
        &self,
        key: &str,
        payload: Value,
        exec: E,
        reconcile: R,
    ) -> std::result::Result<Value, StepError>
    where
        E: FnOnce() -> EF,
        EF: Future<Output = std::result::Result<Value, EffectError>>,
        R: FnOnce(Intent) -> RF,
        RF: Future<Output = Result<Reconciliation>>,
    {
        self.effect_inner(key, payload, None, exec, reconcile).await
    }

    /// Like `effect`, but the effect requires the run's granted approval. The approval hash is
    /// computed here from the exact `payload` that is executed, so an approval for one payload can
    /// never authorize another. The approval is consumed once and bound to this intent: it cannot
    /// authorize a second effect (a different key), and is cleared from the run at checkpoint.
    pub async fn approved_effect<E, EF, R, RF>(
        &self,
        key: &str,
        payload: Value,
        exec: E,
        reconcile: R,
    ) -> std::result::Result<Value, StepError>
    where
        E: FnOnce() -> EF,
        EF: Future<Output = std::result::Result<Value, EffectError>>,
        R: FnOnce(Intent) -> RF,
        RF: Future<Output = Result<Reconciliation>>,
    {
        let id = self.run.approval_id.ok_or_else(|| {
            PairError::new(ErrorCode::ApprovalRequired, "no approval attached to run")
        })?;
        let hash = action_hash(&payload)?;
        let approval = Approval { id, hash };
        self.effect_inner(key, payload, Some(approval), exec, reconcile)
            .await
    }

    async fn effect_inner<E, EF, R, RF>(
        &self,
        key: &str,
        payload: Value,
        approval: Option<Approval>,
        exec: E,
        reconcile: R,
    ) -> std::result::Result<Value, StepError>
    where
        E: FnOnce() -> EF,
        EF: Future<Output = std::result::Result<Value, EffectError>>,
        R: FnOnce(Intent) -> RF,
        RF: Future<Output = Result<Reconciliation>>,
    {
        let run = self.run.id;
        if let Begun::Existing {
            status,
            payload: stored,
            result,
        } = self.begin(key, &payload, approval.as_ref()).await?
        {
            if stored != payload {
                return Err(PairError::new(
                    ErrorCode::Conflict,
                    format!("effect {key} replayed with a different payload"),
                )
                .into());
            }
            if status == COMPLETED {
                return Ok(result.unwrap_or(Value::Null));
            }
            // intended / executing / unknown: the effect may have run. Reconcile, never re-execute blindly.
            self.transition(key, &[INTENDED, EXECUTING, UNKNOWN], UNKNOWN)
                .await?;
            tracing::warn!(run_id = %run, effect = key, "unresolved intent, reconciling");
            let intent = Intent {
                run_id: run,
                key: key.to_owned(),
                payload,
            };
            if let Reconciliation::Applied(result) = reconcile(intent).await? {
                self.finish(key, &result, UNKNOWN).await?;
                return Ok(result);
            }
        }

        // CAS into `executing` under the lease guard: from here on the effect may have happened.
        self.transition(key, &[INTENDED, UNKNOWN], EXECUTING)
            .await?;
        match exec().await {
            Ok(result) => {
                self.finish(key, &result, EXECUTING).await?;
                Ok(result)
            }
            Err(EffectError::Failed(msg)) => {
                self.forget(key).await?;
                Err(StepError::Transient(msg))
            }
            Err(EffectError::Ambiguous(msg)) => {
                self.transition(key, &[EXECUTING], UNKNOWN).await?;
                Err(StepError::Transient(format!(
                    "ambiguous effect {key}: {msg}"
                )))
            }
        }
    }

    /// Insert the intent (and authorize/bind the approval) in one fenced transaction.
    async fn begin(
        &self,
        key: &str,
        payload: &Value,
        approval: Option<&Approval>,
    ) -> Result<Begun> {
        let run = self.run.id;
        let mut tx = self.store.pool.begin().await.map_err(db_err)?;
        JobStore::guard(&mut tx, run, &self.lease).await?;
        if let Some(a) = approval {
            authorize_effect(&mut tx, a.id, &a.hash, run, key).await?;
        }
        let inserted = sqlx::query(
            "INSERT INTO effect_intents (id, run_id, effect_key, status, payload, approval_id) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (run_id, effect_key) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(run.0)
        .bind(key)
        .bind(INTENDED)
        .bind(payload)
        .bind(approval.map(|a| a.id.0))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected()
            == 1;
        let begun = if inserted {
            Begun::Fresh
        } else {
            if let Some(a) = approval {
                bind_approval(&mut tx, run, key, a.id).await?;
            }
            let row = sqlx::query(
                "SELECT status, payload, result FROM effect_intents WHERE run_id = $1 AND effect_key = $2",
            )
            .bind(run.0)
            .bind(key)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            Begun::Existing {
                status: row.try_get("status").map_err(db_err)?,
                payload: row.try_get("payload").map_err(db_err)?,
                result: row.try_get("result").map_err(db_err)?,
            }
        };
        tx.commit().await.map_err(db_err)?;
        Ok(begun)
    }

    /// Fenced status change. Zero rows (lease guard failed, or the row is not in a `from` state
    /// because someone else moved it) means this worker no longer owns the effect: lease lost.
    async fn transition(&self, key: &str, from: &[&str], to: &str) -> Result<()> {
        let mut tx = self.store.pool.begin().await.map_err(db_err)?;
        JobStore::guard(&mut tx, self.run.id, &self.lease).await?;
        let from: Vec<String> = from.iter().map(|s| (*s).to_owned()).collect();
        let n = sqlx::query(
            "UPDATE effect_intents SET status = $3 WHERE run_id = $1 AND effect_key = $2 AND status = ANY($4)",
        )
        .bind(self.run.id.0)
        .bind(key)
        .bind(to)
        .bind(&from)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(lease_lost());
        }
        tx.commit().await.map_err(db_err)
    }

    async fn finish(&self, key: &str, result: &Value, from: &str) -> Result<()> {
        let mut tx = self.store.pool.begin().await.map_err(db_err)?;
        JobStore::guard(&mut tx, self.run.id, &self.lease).await?;
        let n = sqlx::query(
            "UPDATE effect_intents SET status = $3, result = $4, completed_at = now() \
             WHERE run_id = $1 AND effect_key = $2 AND status = $5",
        )
        .bind(self.run.id.0)
        .bind(key)
        .bind(COMPLETED)
        .bind(result)
        .bind(from)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(lease_lost());
        }
        tx.commit().await.map_err(db_err)
    }

    /// Drop an intent whose effect definitely did not happen. Fenced and limited to `executing`
    /// rows, so it can never delete an intent another worker has since taken over.
    async fn forget(&self, key: &str) -> Result<()> {
        let mut tx = self.store.pool.begin().await.map_err(db_err)?;
        JobStore::guard(&mut tx, self.run.id, &self.lease).await?;
        let n = sqlx::query(
            "DELETE FROM effect_intents WHERE run_id = $1 AND effect_key = $2 AND status = $3",
        )
        .bind(self.run.id.0)
        .bind(key)
        .bind(EXECUTING)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(lease_lost());
        }
        tx.commit().await.map_err(db_err)
    }
}

/// Attach `approval` to an existing intent that has none yet.
async fn bind_approval(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run: RunId,
    key: &str,
    approval: ApprovalId,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE effect_intents SET approval_id = $3 \
         WHERE run_id = $1 AND effect_key = $2 AND (approval_id IS NULL OR approval_id = $3)",
    )
    .bind(run.0)
    .bind(key)
    .bind(approval.0)
    .execute(&mut **tx)
    .await
    .map_err(db_err)?
    .rows_affected();
    if n == 0 {
        return Err(PairError::new(
            ErrorCode::Conflict,
            "effect is bound to a different approval",
        ));
    }
    Ok(())
}
