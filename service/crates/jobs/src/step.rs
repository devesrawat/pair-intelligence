use crate::error::StepError;
use crate::lease::Lease;
use crate::records::{RunRecord, StepRecord};
use crate::store::JobStore;
use async_trait::async_trait;
use std::future::Future;

/// What a step wants the worker to do next.
#[derive(Debug, Clone)]
pub enum StepOutcome {
    /// Step done; checkpoint `output` and continue with the next step.
    Next {
        name: String,
        output: serde_json::Value,
    },
    /// Final step; checkpoint and mark the run `succeeded`.
    Finish {
        name: String,
        output: serde_json::Value,
    },
    /// Park the run in `waiting_approval` for this exact payload hash (`action_hash` of the payload
    /// the step will later pass to `StepCtx::approved_effect`). The same step runs again after
    /// `JobStore::grant`.
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
    pub(crate) lease: Lease,
}

impl StepCtx {
    pub(crate) fn new(
        run: RunRecord,
        completed: Vec<StepRecord>,
        store: JobStore,
        lease: Lease,
    ) -> Self {
        let index = run.next_step;
        Self {
            run,
            index,
            completed,
            store,
            lease,
        }
    }

    /// Run a tool call under the run's persisted tool-call cap (20 by default). The counter is
    /// incremented here, before `call` runs, and only while this worker holds the lease; callers
    /// cannot forget to count. At the cap the run fails with `ErrorCode::LimitExceeded`.
    pub async fn gated_tool<T, F, Fut>(&self, call: F) -> Result<T, StepError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, StepError>>,
    {
        if !self
            .store
            .record_tool_call(self.run.id, &self.lease)
            .await?
        {
            return Err(StepError::limit_exceeded(format!(
                "max {} tool calls",
                self.store.cfg.max_tool_calls
            )));
        }
        call().await
    }
}
