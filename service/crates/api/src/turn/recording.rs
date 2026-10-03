//! Decorators handed to `ModelCaller`. `ModelCaller` returns only the final response; these record
//! every attempt (model tried, reservation, settlement) so the turn can persist one `model_calls`
//! row per attempt with its cost state, and they take the persisted attempt slot before each reserve.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use async_trait::async_trait;
use pair_budget::PgBudget;
use pair_core::error::{ErrorCode, Result};
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::Micros;
use pair_core::traits::{Budget, BudgetEx, Provider};
use pair_core::types::{
    BudgetCategory, LedgerEntry, ModelRequest, ModelResponse, ReserveRequest, TaskKind, UsageReport,
};

use crate::attempts::AttemptStore;

#[derive(Debug, Clone)]
pub enum AttemptOutcome {
    Succeeded(ModelResponse),
    Failed { code: ErrorCode, latency_ms: u64 },
}

/// One reserve -> generate -> reconcile cycle.
#[derive(Debug, Clone)]
pub struct AttemptTrace {
    pub reservation: ReservationId,
    pub model_id: Option<String>,
    pub outcome: Option<AttemptOutcome>,
    /// `Some(true)` once the budget settled the reservation; `Some(false)` when it stays unresolved.
    pub settled: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct TurnTrace(Arc<Mutex<Vec<AttemptTrace>>>);

impl TurnTrace {
    fn with<R>(&self, f: impl FnOnce(&mut Vec<AttemptTrace>) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    pub fn snapshot(&self) -> Vec<AttemptTrace> {
        self.with(|v| v.clone())
    }
}

/// The budget as `ModelCaller` sees it: metered reservations first take an attempt slot, so a
/// refused attempt never creates a reservation (the ledger has no release).
pub struct TracedBudget<'a> {
    pub inner: &'a PgBudget,
    pub attempts: &'a AttemptStore,
    pub trace: TurnTrace,
}

#[async_trait]
impl Budget for TracedBudget<'_> {
    async fn reserve(&self, task: TaskId, max_cost: Micros) -> Result<ReservationId> {
        Budget::reserve(self.inner, task, max_cost).await
    }

    async fn reconcile(&self, id: ReservationId, usage: UsageReport) -> Result<LedgerEntry> {
        let entry = Budget::reconcile(self.inner, id, usage).await?;
        self.trace.with(|v| {
            if let Some(t) = v.iter_mut().find(|t| t.reservation == id) {
                t.settled = Some(entry.settled);
            }
        });
        Ok(entry)
    }
}

#[async_trait]
impl BudgetEx for TracedBudget<'_> {
    async fn reserve_with(&self, req: ReserveRequest) -> Result<ReservationId> {
        if req.category == BudgetCategory::Metered {
            let used = self.attempts.consume(req.task).await?;
            tracing::info!(task = %req.task, used, max = self.attempts.max_attempts(), "model attempt taken");
        }
        let id = BudgetEx::reserve_with(self.inner, req).await?;
        self.trace.with(|v| {
            v.push(AttemptTrace {
                reservation: id,
                model_id: None,
                outcome: None,
                settled: None,
            });
        });
        Ok(id)
    }

    fn task_cap(&self, kind: TaskKind) -> Micros {
        BudgetEx::task_cap(self.inner, kind)
    }
}

/// Records which model each provider call targeted and how it ended.
pub struct TracedProvider<'a> {
    pub inner: &'a dyn Provider,
    pub trace: TurnTrace,
}

#[async_trait]
impl Provider for TracedProvider<'_> {
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse> {
        let model_id = req.model_id.clone();
        let started = Instant::now();
        let result = self.inner.generate(req).await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let outcome = match &result {
            Ok(resp) => AttemptOutcome::Succeeded(resp.clone()),
            Err(e) => AttemptOutcome::Failed {
                code: e.code,
                latency_ms,
            },
        };
        self.trace.with(|v| {
            if let Some(last) = v.last_mut() {
                last.model_id = Some(model_id);
                last.outcome = Some(outcome);
            }
        });
        result
    }
}
