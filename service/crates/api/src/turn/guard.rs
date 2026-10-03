//! A turn that is dropped between reserve and reconcile must not leave a `held` reservation: the
//! actual cost is unknown, so it is settled as unresolved (counted in full), never as zero.

use std::sync::Arc;

use pair_budget::PgBudget;
use pair_core::ids::TaskId;
use pair_core::traits::Budget;
use pair_core::types::UsageReport;
use tokio::runtime::Handle;
use tokio_util::task::TaskTracker;

use super::claim::ClaimStore;
use super::recording::TurnTrace;

/// Ledger price version when the reservation's own cannot be read.
const UNKNOWN_PRICE_VERSION: &str = "unknown";

/// Mark every attempt that reserved but never reconciled as unresolved. Idempotent: replaying an
/// unresolved report maps to the same ledger row.
pub async fn settle_dangling(budget: &PgBudget, trace: &TurnTrace) {
    for attempt in trace.snapshot() {
        if attempt.settled.is_some() {
            continue;
        }
        let price_version = match budget.reservation_binding(attempt.reservation).await {
            Ok(binding) => binding.price_version,
            Err(_) => UNKNOWN_PRICE_VERSION.to_owned(),
        };
        let unknown = UsageReport {
            input_tokens: 0,
            output_tokens: 0,
            actual_cost: None,
            price_version,
        };
        match Budget::reconcile(budget, attempt.reservation, unknown).await {
            Ok(_) => tracing::warn!(
                reservation = %attempt.reservation,
                "a turn ended before its reservation was settled; left unresolved"
            ),
            Err(e) => tracing::error!(
                reservation = %attempt.reservation, error = %e,
                "could not mark a dangling reservation unresolved"
            ),
        }
    }
}

/// Armed for the paid part of a turn. If the turn's future is dropped (task aborted, runtime
/// shutting down) the cleanup runs on the turn tracker, so the shutdown drain waits for it.
pub struct TurnGuard {
    budget: Arc<PgBudget>,
    trace: TurnTrace,
    tracker: TaskTracker,
    claim: Option<(ClaimStore, TaskId)>,
    armed: bool,
}

impl TurnGuard {
    pub fn arm(
        budget: Arc<PgBudget>,
        trace: TurnTrace,
        tracker: TaskTracker,
        claim: Option<(ClaimStore, TaskId)>,
    ) -> Self {
        Self {
            budget,
            trace,
            tracker,
            claim,
            armed: true,
        }
    }

    /// The turn ran to its end and did its own bookkeeping.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let budget = Arc::clone(&self.budget);
        let trace = self.trace.clone();
        let claim = self.claim.take();
        let cleanup = async move {
            settle_dangling(&budget, &trace).await;
            if let Some((claims, task)) = claim {
                claims.finish(task, false).await;
            }
        };
        match Handle::try_current() {
            Ok(handle) => {
                self.tracker.spawn_on(cleanup, &handle);
            }
            Err(_) => tracing::error!("turn dropped outside a runtime: reservation left held"),
        }
    }
}
