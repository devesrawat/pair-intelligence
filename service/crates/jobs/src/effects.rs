//! Durable intent records for external side effects (send, push, ...).
//!
//! Protocol: the intent row is written BEFORE the effect and completed after. If the process dies
//! in between, the row stays `intended`; on resume it is marked `unknown` and the caller-supplied
//! reconcile callback decides whether the effect happened. The effect is never re-run blindly.
use crate::error::{db_err, StepError};
use crate::step::StepCtx;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::RunId;
use serde_json::Value;
use sqlx::Row;
use std::future::Future;
use uuid::Uuid;

const INTENDED: &str = "intended";
const UNKNOWN: &str = "unknown";
const COMPLETED: &str = "completed";

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

impl StepCtx {
    /// Run `exec` at most once per `(run, key)` across crashes and resumes.
    pub async fn effect<E, EF, R, RF>(&self, key: &str, payload: Value, exec: E, reconcile: R) -> std::result::Result<Value, StepError>
    where
        E: FnOnce() -> EF,
        EF: Future<Output = std::result::Result<Value, EffectError>>,
        R: FnOnce(Intent) -> RF,
        RF: Future<Output = Result<Reconciliation>>,
    {
        let run = self.run.id;
        let pool = &self.store.pool;
        let inserted = sqlx::query(
            "INSERT INTO effect_intents (id, run_id, effect_key, status, payload) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (run_id, effect_key) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(run.0)
        .bind(key)
        .bind(INTENDED)
        .bind(&payload)
        .execute(pool)
        .await
        .map_err(db_err)?
        .rows_affected()
            == 1;

        if !inserted {
            let row = sqlx::query("SELECT status, payload, result FROM effect_intents WHERE run_id = $1 AND effect_key = $2")
                .bind(run.0)
                .bind(key)
                .fetch_one(pool)
                .await
                .map_err(db_err)?;
            let status: String = row.try_get("status").map_err(db_err)?;
            let stored: Value = row.try_get("payload").map_err(db_err)?;
            if stored != payload {
                return Err(PairError::new(ErrorCode::Conflict, format!("effect {key} replayed with a different payload")).into());
            }
            if status == COMPLETED {
                let result: Option<Value> = row.try_get("result").map_err(db_err)?;
                return Ok(result.unwrap_or(Value::Null));
            }
            // intended or unknown: the effect may have run. Reconcile, never blindly re-execute.
            self.set_status(key, UNKNOWN).await?;
            tracing::warn!(run_id = %run, effect = key, "unresolved intent, reconciling");
            match reconcile(Intent { run_id: run, key: key.to_owned(), payload }).await? {
                Reconciliation::Applied(result) => {
                    self.complete(key, &result).await?;
                    return Ok(result);
                }
                Reconciliation::NotApplied => self.set_status(key, INTENDED).await?,
            }
        }

        match exec().await {
            Ok(result) => {
                self.complete(key, &result).await?;
                Ok(result)
            }
            Err(EffectError::Failed(msg)) => {
                sqlx::query("DELETE FROM effect_intents WHERE run_id = $1 AND effect_key = $2 AND status = $3")
                    .bind(run.0)
                    .bind(key)
                    .bind(INTENDED)
                    .execute(pool)
                    .await
                    .map_err(db_err)?;
                Err(StepError::Transient(msg))
            }
            Err(EffectError::Ambiguous(msg)) => {
                self.set_status(key, UNKNOWN).await?;
                Err(StepError::Transient(format!("ambiguous effect {key}: {msg}")))
            }
        }
    }

    async fn set_status(&self, key: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE effect_intents SET status = $3 WHERE run_id = $1 AND effect_key = $2")
            .bind(self.run.id.0)
            .bind(key)
            .bind(status)
            .execute(&self.store.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn complete(&self, key: &str, result: &Value) -> Result<()> {
        sqlx::query(
            "UPDATE effect_intents SET status = $3, result = $4, completed_at = now() WHERE run_id = $1 AND effect_key = $2",
        )
        .bind(self.run.id.0)
        .bind(key)
        .bind(COMPLETED)
        .bind(result)
        .execute(&self.store.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }
}
