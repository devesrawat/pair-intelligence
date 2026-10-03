//! `PgBudget`: the Postgres-backed implementation of `pair_core::traits::Budget`.
use crate::{BudgetConfig, PriceBook};
use async_trait::async_trait;
use chrono::Utc;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::Micros;
use pair_core::traits::{Budget, BudgetEx};
use pair_core::types::{BudgetCategory, LedgerEntry, ReserveRequest, TaskKind, UsageReport};
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct PgBudget {
    pub(crate) pool: PgPool,
    pub(crate) config: BudgetConfig,
    pub(crate) prices: PriceBook,
}

impl PgBudget {
    pub fn new(pool: PgPool, config: BudgetConfig, prices: PriceBook) -> Self {
        Self {
            pool,
            config,
            prices,
        }
    }

    /// Explicit version if given, else the book's current one.
    pub(crate) fn resolve_price_version(&self, requested: Option<&str>) -> Result<String> {
        match requested.or_else(|| self.prices.current()) {
            Some(v) => Ok(v.to_owned()),
            None => Err(PairError::new(
                ErrorCode::BudgetUnknownPrice,
                "no current price version configured",
            )),
        }
    }

    pub fn config(&self) -> &BudgetConfig {
        &self.config
    }
}

pub(crate) fn db_err(context: &str, e: sqlx::Error) -> PairError {
    tracing::error!(error = %e, context, "budget database error");
    PairError::new(
        ErrorCode::Internal,
        format!("budget database error during {context}"),
    )
}

#[async_trait]
impl Budget for PgBudget {
    /// Reserve for a task WITHOUT stating its kind. A task already registered via `reserve_with`
    /// keeps its kind and cap; an unregistered task is a `TaskKind::Default` task and gets the
    /// default (smallest) task cap. Coding and research callers must register the task first with
    /// `BudgetEx::reserve_with` (the first reservation fixes the kind).
    async fn reserve(&self, task: TaskId, max_cost: Micros) -> Result<ReservationId> {
        self.reserve_inheriting_kind(ReserveRequest {
            task,
            max_cost,
            kind: TaskKind::Default,
            category: BudgetCategory::Metered,
            price_version: None,
        })
        .await
    }

    async fn reconcile(&self, id: ReservationId, usage: UsageReport) -> Result<LedgerEntry> {
        Ok(self.reconcile_at(id, usage, Utc::now()).await?.entry)
    }
}

#[async_trait]
impl BudgetEx for PgBudget {
    async fn reserve_with(&self, req: ReserveRequest) -> Result<ReservationId> {
        PgBudget::reserve_with(self, req).await
    }
}
