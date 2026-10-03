//! Background tasks hosted by the binary (jobs worker, lease sweeper, orphan reconciler), stopped
//! by the same shutdown signal as the HTTP server with a bounded drain.

mod liveness;
pub mod tasks;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use pair_core::error::{ErrorCode, PairError, Result};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

pub use liveness::{Liveness, TaskStatus, STALL_INTERVALS};

pub type Work = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

/// One periodic task. `work` runs once per interval, and on demand through
/// [`Background::tick_now`] (which is how tests drive it without sleeping).
pub struct TaskSpec {
    pub name: &'static str,
    pub interval: Duration,
    /// How long without a successful tick before `/readyz` calls the task stalled.
    pub stall_after: Duration,
    pub work: Work,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub drained: Vec<&'static str>,
    /// Tasks still running when the grace period ended; they were aborted.
    pub aborted: Vec<&'static str>,
}

type Trigger = oneshot::Sender<()>;

pub struct Background {
    stop: watch::Sender<bool>,
    handles: Vec<(&'static str, JoinHandle<()>)>,
    triggers: BTreeMap<&'static str, mpsc::Sender<Trigger>>,
    liveness: Arc<Liveness>,
}

impl Background {
    pub fn start(specs: Vec<TaskSpec>, liveness: Arc<Liveness>) -> Self {
        let (stop, _) = watch::channel(false);
        let mut handles = Vec::with_capacity(specs.len());
        let mut triggers = BTreeMap::new();
        for spec in specs {
            let name = spec.name;
            liveness.register(name, spec.stall_after);
            let (tx, rx) = mpsc::channel(1);
            triggers.insert(name, tx);
            let handle = tokio::spawn(run_loop(spec, stop.subscribe(), rx, liveness.clone()));
            handles.push((name, handle));
        }
        Self {
            stop,
            handles,
            triggers,
            liveness,
        }
    }

    pub fn liveness(&self) -> Arc<Liveness> {
        self.liveness.clone()
    }

    /// Run `name` once, now, and wait for that run to finish.
    pub async fn tick_now(&self, name: &str) -> Result<()> {
        let unknown = || PairError::new(ErrorCode::NotFound, format!("no background task {name}"));
        let tx = self.triggers.get(name).ok_or_else(unknown)?;
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(ack_tx).await.map_err(|_| {
            PairError::new(ErrorCode::Conflict, format!("background task {name} stopped"))
        })?;
        ack_rx.await.map_err(|_| {
            PairError::new(ErrorCode::Conflict, format!("background task {name} stopped"))
        })
    }

    /// Ask every task to stop after its current iteration. Does not wait.
    pub fn signal_stop(&self) {
        let _ = self.stop.send(true);
    }

    /// Signal stop, then wait up to `grace` for the tasks to finish their current iteration; any
    /// task still running after that is aborted (its jobs lease expires and the sweeper of the
    /// next process requeues the run).
    pub async fn drain(self, grace: Duration) -> DrainReport {
        self.signal_stop();
        let deadline = tokio::time::Instant::now() + grace;
        let mut report = DrainReport::default();
        for (name, mut handle) in self.handles {
            match tokio::time::timeout_at(deadline, &mut handle).await {
                Ok(_) => report.drained.push(name),
                Err(_) => {
                    handle.abort();
                    tracing::warn!(task = name, "background task did not stop within the drain grace; aborted");
                    report.aborted.push(name);
                }
            }
        }
        report
    }
}

async fn run_loop(
    spec: TaskSpec,
    mut stop: watch::Receiver<bool>,
    mut triggers: mpsc::Receiver<Trigger>,
    liveness: Arc<Liveness>,
) {
    loop {
        if *stop.borrow() {
            break;
        }
        let ack = tokio::select! {
            biased;
            _ = stop.changed() => break,
            t = triggers.recv() => t,
            () = tokio::time::sleep(spec.interval) => None,
        };
        match (spec.work)().await {
            Ok(()) => liveness.tick_ok(spec.name),
            Err(e) => {
                tracing::warn!(task = spec.name, code = ?e.code, error = %e.message, "background tick failed");
                liveness.tick_err(spec.name, e.message);
            }
        }
        if let Some(ack) = ack {
            let _ = ack.send(());
        }
    }
    tracing::info!(task = spec.name, "background task stopped");
}
