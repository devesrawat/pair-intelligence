//! PostgreSQL implementation of the gate's audit sink (`tool_executions`, migration 090).
use crate::error::db_err;
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::ToolExecutionId;
use pair_core::types::DataClass;
use pair_policy::recorder::{ExecOutcome, ExecutionRecord, ExecutionRecorder};
use sqlx::PgPool;

/// Writes one `tool_executions` row per gate call. Cheap to clone.
#[derive(Clone)]
pub struct PgExecutionRecorder {
    pool: PgPool,
}

impl PgExecutionRecorder {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn data_class_str(class: DataClass) -> &'static str {
    match class {
        DataClass::Public => "public",
        DataClass::Personal => "personal",
        DataClass::Sensitive => "sensitive",
        DataClass::Employer => "employer",
    }
}

#[async_trait]
impl ExecutionRecorder for PgExecutionRecorder {
    async fn record(&self, row: &ExecutionRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO tool_executions (id, task_id, trace_id, tool, executable, args_hash, \
             destination, data_class, policy_version, decision, approval_id, outcome, error_code, \
             finished_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, \
                     CASE WHEN $12 = 'started' THEN NULL ELSE now() END)",
        )
        .bind(row.id.0)
        .bind(row.task.0)
        .bind(row.trace.0)
        .bind(&row.tool)
        .bind(&row.executable)
        .bind(&row.args_hash)
        .bind(&row.destination)
        .bind(data_class_str(row.data_class))
        .bind(&row.policy_version)
        .bind(row.decision.as_str())
        .bind(row.approval.map(|a| a.0))
        .bind(row.outcome.as_str())
        .bind(&row.error_code)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn finish(
        &self,
        id: ToolExecutionId,
        outcome: ExecOutcome,
        error_code: Option<&str>,
    ) -> Result<()> {
        if !outcome.is_terminal() {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "finish needs a terminal outcome",
            ));
        }
        let n = sqlx::query(
            "UPDATE tool_executions SET outcome = $2, error_code = $3, finished_at = now() \
             WHERE id = $1 AND outcome = 'started'",
        )
        .bind(id.0)
        .bind(outcome.as_str())
        .bind(error_code)
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(PairError::new(
                ErrorCode::NotFound,
                format!("no started execution {id}"),
            ));
        }
        Ok(())
    }
}
