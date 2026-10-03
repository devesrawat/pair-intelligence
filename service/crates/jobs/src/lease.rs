//! Lease identity and the background keeper that renews it while a step executes.
use crate::error::is_lease_lost;
use crate::store::JobStore;
use pair_core::ids::RunId;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Floor for the heartbeat period so a tiny TTL cannot spin the keeper.
const MIN_HEARTBEAT: Duration = Duration::from_millis(10);
/// The lease is renewed this many times per TTL.
const HEARTBEATS_PER_TTL: u32 = 3;

/// Identity of one claim of a run. `epoch` increments on every claim, so a worker that outlived
/// its lease can never match the run row again, even if the same worker id re-claims.
#[derive(Debug, Clone)]
pub(crate) struct Lease {
    pub owner: String,
    pub epoch: i64,
}

/// Renews the lease every `ttl / 3` on its own task and flags loss. Dropping it stops renewal.
pub(crate) struct LeaseKeeper {
    lost: watch::Receiver<bool>,
    handle: JoinHandle<()>,
}

impl LeaseKeeper {
    pub fn start(store: JobStore, run: RunId, lease: Lease) -> Self {
        let ttl = store.config().lease_ttl;
        let period = (ttl / HEARTBEATS_PER_TTL).max(MIN_HEARTBEAT);
        let (tx, lost) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut last_ok = Instant::now();
            loop {
                tokio::time::sleep(period).await;
                match store.heartbeat(run, &lease).await {
                    Ok(()) => last_ok = Instant::now(),
                    Err(e) if is_lease_lost(&e) => {
                        tracing::warn!(run_id = %run, worker = %lease.owner, "lease lost");
                        let _ = tx.send(true);
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(run_id = %run, error = %e, "heartbeat failed");
                        if last_ok.elapsed() >= ttl {
                            let _ = tx.send(true);
                            return;
                        }
                    }
                }
            }
        });
        Self { lost, handle }
    }

    /// Resolves when the lease is lost (or the keeper died, which is treated as lost).
    pub async fn lost(&self) {
        let mut rx = self.lost.clone();
        let _ = rx.wait_for(|lost| *lost).await;
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
