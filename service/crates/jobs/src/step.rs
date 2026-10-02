use crate::approvals::consume_inner;
use crate::error::StepError;
use crate::records::{RunRecord, StepRecord};
use crate::store::JobStore;
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError};

/// What a step wants the worker to do next.
#[derive(Debug, Clone)]
pub enum StepOutcome {
    /// Step done; checkpoint `output` and continue with the next step.
    Next { name: String, output: serde_json::Value },
    /// Final step; checkpoint and mark the run `succeeded`.
    Finish { name: String, output: serde_json::Value },
    /// Park the run in `waiting_approval` for this exact payload hash. The same step runs again
    /// after `JobStore::grant`.
    AwaitApproval { action_hash: String },
}

/// Caller-supplied workflow logic for one run `kind`. `step` is invoked once per step index and
/// must be safe to re-run after a crash: use `StepCtx::effect` for external side effects.
#[async_trait]
pub trait StepHandler: Send + Sync {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError>;
}

/// Context for the step being executed.
pub struct StepCtx {
    pub run: RunRecord,
    /// Index of the step being executed (equals `run.next_step`).
    pub index: u32,
    /// Checkpoints of steps already completed, in order.
    pub completed: Vec<StepRecord>,
    pub(crate) store: JobStore,
}

impl StepCtx {
    pub(crate) fn new(run: RunRecord, completed: Vec<StepRecord>, store: JobStore) -> Self {
        let index = run.next_step;
        Self { run, index, completed, store }
    }

    /// Count a tool call against the run's cap (20 by default, persisted across resumes).
    pub async fn record_tool_call(&self) -> Result<(), StepError> {
        if self.store.record_tool_call(self.run.id).await? {
            Ok(())
        } else {
            Err(StepError::LimitExceeded(format!("max {} tool calls", self.store.cfg.max_tool_calls)))
        }
    }

    /// Recheck the granted approval at execution time and consume it. Fails if the payload hash
    /// differs, the approval expired, or it was consumed by another caller.
    pub async fn consume_approval(&self, action_hash: &str) -> Result<(), StepError> {
        let id = self
            .run
            .approval_id
            .ok_or_else(|| PairError::new(ErrorCode::ApprovalRequired, "no approval attached to run"))?;
        consume_inner(&self.store.pool, id, action_hash, Some(self.run.id)).await?;
        Ok(())
    }
}
