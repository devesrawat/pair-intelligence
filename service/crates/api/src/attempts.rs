//! Persisted per-task model-attempt cap (spec section 6: 3 attempts per task in total).

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::TaskId;
use sqlx::PgPool;

fn db_err(e: &sqlx::Error) -> PairError {
    tracing::error!(error = %e, "attempt counter database error");
    PairError::new(ErrorCode::Internal, "attempt counter database error")
}

/// Consumed before each provider call and stored in `task_model_attempts`, so step retries,
/// HTTP retries and restarts all draw from the same counter.
#[derive(Clone)]
pub struct AttemptStore {
    pool: PgPool,
    max_attempts: u32,
}

impl AttemptStore {
    pub fn new(pool: PgPool, max_attempts: u32) -> Self {
        Self { pool, max_attempts }
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Atomically take one attempt for `task`. Returns the attempts used after this one, or
    /// `BudgetExceeded` (never a provider error, which would make the caller try another model)
    /// once the cap is spent. One statement: concurrent callers cannot both take the last slot.
    pub async fn consume(&self, task: TaskId) -> Result<u32> {
        let cap = i32::try_from(self.max_attempts).unwrap_or(i32::MAX);
        let used: Option<i32> = sqlx::query_scalar(
            "INSERT INTO task_model_attempts AS t (task_id, used) \
             SELECT $1, 1 WHERE $2 >= 1 \
             ON CONFLICT (task_id) DO UPDATE SET used = t.used + 1, updated_at = now() \
             WHERE t.used < $2 \
             RETURNING t.used",
        )
        .bind(task.0)
        .bind(cap)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| db_err(&e))?;
        match used {
            Some(n) => Ok(u32::try_from(n).unwrap_or(u32::MAX)),
            None => Err(PairError::new(
                ErrorCode::BudgetExceeded,
                format!(
                    "task {task} has used its {} model attempts; no further provider calls",
                    self.max_attempts
                ),
            )),
        }
    }

    pub async fn used(&self, task: TaskId) -> Result<u32> {
        let used: Option<i32> =
            sqlx::query_scalar("SELECT used FROM task_model_attempts WHERE task_id = $1")
                .bind(task.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| db_err(&e))?;
        Ok(used.map_or(0, |n| u32::try_from(n).unwrap_or(0)))
    }
}
