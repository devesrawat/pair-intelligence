//! `PgBudget`: the Postgres-backed implementation of `pair_core::traits::Budget`.
use crate::reserve::{ReserveRequest, TaskKind};
use crate::{BudgetConfig, PriceBook};
use async_trait::async_trait;
use chrono::Utc;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::Micros;
use pair_core::traits::Budget;
use pair_core::types::{LedgerEntry, UsageReport};
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct PgBudget {
    pub(crate) pool: PgPool,
    pub(crate) config: BudgetConfig,
    pub(crate) prices: PriceBook,
}

impl PgBudget {
    pub fn new(pool: PgPool, config: BudgetConfig, prices: PriceBook) -> Self {
        Self { pool, config, prices }
    }

    pub fn config(&self) -> &BudgetConfig {
        &self.config
    }
}

pub(crate) fn db_err(context: &str, e: sqlx::Error) -> PairError {
    tracing::error!(error = %e, context, "budget database error");
    PairError::new(ErrorCode::Internal, format!("budget database error during {context}"))
}

#[async_trait]
impl Budget for PgBudget {
    async fn reserve(&self, task: TaskId, max_cost: Micros) -> Result<ReservationId> {
        let price_version = self
            .prices
            .current()
            .ok_or_else(|| PairError::new(ErrorCode::BudgetUnknownPrice, "no current price version configured"))?
            .to_owned();
        self.reserve_with(ReserveRequest::metered(task, max_cost, TaskKind::Default, price_version)).await
    }

    async fn reconcile(&self, id: ReservationId, usage: UsageReport) -> Result<LedgerEntry> {
        self.reconcile_at(id, usage, Utc::now()).await
    }
}
