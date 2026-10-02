use crate::error::{is_lease_lost, StepError};
use crate::records::RunRecord;
use crate::step::{StepCtx, StepHandler, StepOutcome};
use crate::store::JobStore;
use chrono::Utc;
use pair_core::error::Result;
use pair_core::ids::RunId;
use pair_core::types::RunState;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const DEADLINE_FAILURE: &str = "deadline_exceeded";

/// Claims runs under a lease and drives them step by step, checkpointing after each step.
/// Dropping a worker mid-run leaves the lease to expire; `JobStore::sweep_expired` then marks the
/// run interrupted and `resume` makes it claimable again.
pub struct Worker {
    store: JobStore,
    id: String,
    handlers: HashMap<String, Arc<dyn StepHandler>>,
}

impl Worker {
    pub fn new(store: JobStore, id: impl Into<String>) -> Self {
        Self {
            store,
            id: id.into(),
            handlers: HashMap::new(),
        }
    }

    pub fn with_handler(mut self, kind: impl Into<String>, handler: Arc<dyn StepHandler>) -> Self {
        self.handlers.insert(kind.into(), handler);
        self
    }

    /// Claim one run and drive it until it finishes, fails, or parks. `None` when nothing is queued.
    pub async fn run_once(&self) -> Result<Option<(RunId, RunState)>> {
        let Some(run) = self.store.claim(&self.id).await? else {
            return Ok(None);
        };
        let id = run.id;
        tracing::info!(run_id = %id, worker = %self.id, kind = %run.kind, "run claimed");
        let state = match self.drive(run).await {
            Ok(state) => state,
            Err(e) if is_lease_lost(&e) => {
                tracing::warn!(run_id = %id, worker = %self.id, "lease lost, abandoning run");
                self.store.state(id).await?
            }
            Err(e) => return Err(e),
        };
        Ok(Some((id, state)))
    }

    async fn drive(&self, mut run: RunRecord) -> Result<RunState> {
        let Some(handler) = self.handlers.get(&run.kind).cloned() else {
            return self
                .fail(&run, &format!("no handler for kind {}", run.kind))
                .await;
        };
        loop {
            let now = Utc::now();
            if now >= run.deadline_at {
                return self.fail(&run, DEADLINE_FAILURE).await;
            }
            self.store.heartbeat(run.id, &self.id).await?;
            let completed = self.store.steps(run.id).await?;
            let ctx = StepCtx::new(run.clone(), completed, self.store.clone());
            let remaining = (run.deadline_at - now).to_std().unwrap_or(Duration::ZERO);
            let outcome =
                match tokio::time::timeout(remaining, self.step_with_retry(handler.as_ref(), &ctx))
                    .await
                {
                    Err(_elapsed) => return self.fail(&run, DEADLINE_FAILURE).await,
                    Ok(Err(e)) => return self.fail(&run, &e.to_string()).await,
                    Ok(Ok(outcome)) => outcome,
                };
            match outcome {
                StepOutcome::Next { name, output } => {
                    self.store
                        .checkpoint(run.id, &self.id, ctx.index, &name, &output, None)
                        .await?;
                    run = RunRecord {
                        next_step: ctx.index + 1,
                        ..run
                    };
                }
                StepOutcome::Finish { name, output } => {
                    self.store
                        .checkpoint(run.id, &self.id, ctx.index, &name, &output, Some(&output))
                        .await?;
                    return Ok(RunState::Succeeded);
                }
                StepOutcome::AwaitApproval { action_hash } => {
                    let to = RunState::WaitingApproval;
                    self.store
                        .release(run.id, &self.id, to, None, Some(&action_hash))
                        .await?;
                    return Ok(to);
                }
            }
        }
    }

    /// Retries only `StepError::Transient`, with capped exponential backoff from `RetryPolicy`.
    async fn step_with_retry(
        &self,
        handler: &dyn StepHandler,
        ctx: &StepCtx,
    ) -> std::result::Result<StepOutcome, StepError> {
        let policy = self.store.config().retry;
        let mut attempt = 1;
        loop {
            match handler.step(ctx).await {
                Err(StepError::Transient(msg)) if attempt < policy.max_attempts => {
                    let delay = policy.delay(attempt);
                    tracing::warn!(run_id = %ctx.run.id, attempt, ?delay, error = %msg, "transient step error, backing off");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    async fn fail(&self, run: &RunRecord, reason: &str) -> Result<RunState> {
        tracing::error!(run_id = %run.id, reason, "run failed");
        self.store
            .release(run.id, &self.id, RunState::Failed, Some(reason), None)
            .await?;
        Ok(RunState::Failed)
    }
}
