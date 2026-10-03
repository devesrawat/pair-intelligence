//! The concrete background tasks: jobs worker loop, lease sweeper, orphaned-intent reconciler.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_jobs::{Intent, IntentReconciler, JobStore, Reconciliation, StepHandler, Worker};

use super::{TaskSpec, Work, STALL_INTERVALS};

pub const WORKER_TASK: &str = "jobs_worker";
pub const SWEEPER_TASK: &str = "lease_sweeper";
pub const ORPHANS_TASK: &str = "orphan_reconciler";

pub const DEFAULT_WORKER_INTERVAL: Duration = Duration::from_secs(2);
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(15);
pub const DEFAULT_ORPHAN_INTERVAL: Duration = Duration::from_secs(60);
pub const DEFAULT_DRAIN_GRACE: Duration = Duration::from_secs(20);
/// One tick of the worker can drive a run to its wall limit (30 minutes for research), so only a
/// tick that outlives that twice over counts as stalled.
const WORKER_STALL_AFTER: Duration = Duration::from_secs(2 * 30 * 60);
const WORKER_ID_PREFIX: &str = "pair-api";

#[derive(Debug, Clone, Copy)]
pub struct BackgroundIntervals {
    pub worker: Duration,
    pub sweeper: Duration,
    pub orphans: Duration,
}

impl Default for BackgroundIntervals {
    fn default() -> Self {
        Self {
            worker: DEFAULT_WORKER_INTERVAL,
            sweeper: DEFAULT_SWEEP_INTERVAL,
            orphans: DEFAULT_ORPHAN_INTERVAL,
        }
    }
}

/// Never guesses whether an orphaned effect happened: every intent stays unresolved (and is
/// retried by the next sweep) until a reconciler that can actually ask the remote system exists.
pub struct UndecidedReconciler;

#[async_trait]
impl IntentReconciler for UndecidedReconciler {
    async fn reconcile(&self, intent: Intent) -> Result<Reconciliation> {
        Err(PairError::new(
            ErrorCode::Conflict,
            format!(
                "undecided: no reconciler can verify effect {} of run {}",
                intent.key, intent.run_id
            ),
        ))
    }
}

fn work<F, Fut>(f: F) -> Work
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    Arc::new(move || Box::pin(f()))
}

/// Marks runs whose lease lapsed as interrupted and makes them claimable again.
pub fn sweeper(store: JobStore, interval: Duration) -> TaskSpec {
    TaskSpec {
        name: SWEEPER_TASK,
        interval,
        stall_after: interval * STALL_INTERVALS,
        work: work(move || {
            let store = store.clone();
            async move {
                store.sweep_expired().await?;
                store.requeue_interrupted().await?;
                Ok(())
            }
        }),
    }
}

pub fn orphan_reconciler(store: JobStore, interval: Duration) -> TaskSpec {
    TaskSpec {
        name: ORPHANS_TASK,
        interval,
        stall_after: interval * STALL_INTERVALS,
        work: work(move || {
            let store = store.clone();
            async move {
                let sweep = store.reconcile_orphaned_intents(&UndecidedReconciler).await?;
                if sweep.undecided > 0 {
                    tracing::warn!(
                        undecided = sweep.undecided,
                        "orphaned effect intents remain unresolved: no reconciler can decide them"
                    );
                }
                Ok(())
            }
        }),
    }
}

/// Drives queued runs. Returns `None` without handlers: `JobStore::claim` is not filtered by kind,
/// and a worker without a handler fails every run it claims, so a handler-less worker would
/// destroy queued runs. The service registers none today (the workflow handlers need a repository
/// and a sandbox); the loop exists so that registering one is all it takes.
pub fn worker_loop(
    store: JobStore,
    handlers: Vec<(String, Arc<dyn StepHandler>)>,
    interval: Duration,
) -> Option<TaskSpec> {
    if handlers.is_empty() {
        return None;
    }
    let worker_id = format!("{WORKER_ID_PREFIX}-{}", uuid::Uuid::new_v4().simple());
    let worker = Arc::new(
        handlers
            .into_iter()
            .fold(Worker::new(store, worker_id), |w, (kind, h)| {
                w.with_handler(kind, h)
            }),
    );
    Some(TaskSpec {
        name: WORKER_TASK,
        interval,
        stall_after: WORKER_STALL_AFTER,
        work: work(move || {
            let worker = worker.clone();
            async move {
                while worker.run_once().await?.is_some() {}
                Ok(())
            }
        }),
    })
}
