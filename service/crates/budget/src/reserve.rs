//! Reservation: check every cap under one advisory lock, then insert.
use crate::ledger::{db_err, PgBudget};
use crate::period::Period;
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::Micros;
use pair_core::types::{BudgetCategory, ReserveRequest, TaskKind};

/// Single global lock key serializing all reservations AND reconciliations (single-owner system).
/// Both take it, so a settlement that raises a reservation's counted amount (overrun) can never
/// interleave with a reserve that has already checked the caps.
pub const RESERVE_LOCK_KEY: i64 = 0x5041_4952_4255_4447; // "PAIRBUDG"

struct Totals {
    task: i64,
    day: i64,
    month: i64,
    classifier_month: i64,
}

fn exceeded(what: &str) -> PairError {
    PairError::new(
        ErrorCode::BudgetExceeded,
        format!("{what} cap would be exceeded"),
    )
}

fn within(counted: i64, add: Micros, cap: Micros, what: &str) -> Result<()> {
    let total = Micros(counted)
        .checked_add(add)
        .ok_or_else(|| PairError::new(ErrorCode::InvalidInput, "amount overflow"))?;
    if total > cap {
        return Err(exceeded(what));
    }
    Ok(())
}

impl PgBudget {
    /// Reserve with an explicit task kind. The first reservation for a task fixes its kind; a later
    /// reservation of a different kind is a `Conflict`.
    pub async fn reserve_with(&self, req: ReserveRequest) -> Result<ReservationId> {
        self.reserve_at(req, false, Utc::now()).await
    }

    /// Reserve for a task whose kind the caller does not state: an already-registered task keeps
    /// its kind (and cap); an unregistered one becomes `TaskKind::Default`.
    pub(crate) async fn reserve_inheriting_kind(
        &self,
        req: ReserveRequest,
    ) -> Result<ReservationId> {
        self.reserve_at(req, true, Utc::now()).await
    }

    pub(crate) async fn reserve_at(
        &self,
        mut req: ReserveRequest,
        inherit_kind: bool,
        now: DateTime<Utc>,
    ) -> Result<ReservationId> {
        let price_version = self.resolve_price_version(req.price_version.as_deref())?;
        if !self.prices.is_known(&price_version) {
            return Err(PairError::new(
                ErrorCode::BudgetUnknownPrice,
                format!("unknown price version {price_version:?}"),
            ));
        }
        if req.max_cost <= Micros::ZERO {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "max_cost must be positive",
            ));
        }
        let period = Period::at(now);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| db_err("begin reserve", e))?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(RESERVE_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_err("advisory lock", e))?;

        req.kind = register_task_kind(&mut tx, &req, inherit_kind, now).await?;
        let t = totals(&mut tx, req.task, period).await?;
        within(t.task, req.max_cost, self.config.task_cap(req.kind), "task")?;
        within(t.day, req.max_cost, self.config.daily_cap(), "daily")?;
        within(t.month, req.max_cost, self.config.monthly_cap(), "monthly")?;
        if req.category == BudgetCategory::Classifier {
            within(
                t.classifier_month,
                req.max_cost,
                self.config.classifier_monthly_subcap(),
                "classifier",
            )?;
        }

        let id = ReservationId::new();
        sqlx::query(
            "INSERT INTO budget_reservations \
             (id, task_id, category, task_kind, price_version, period_day, period_month, \
              reserved_micros, counted_micros, state, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, 'held', $9)",
        )
        .bind(id.0)
        .bind(req.task.0)
        .bind(req.category.as_str())
        .bind(req.kind.as_str())
        .bind(&price_version)
        .bind(period.day)
        .bind(period.month)
        .bind(req.max_cost.0)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_err("insert reservation", e))?;
        tx.commit().await.map_err(|e| db_err("commit reserve", e))?;
        tracing::info!(reservation = %id, task = %req.task, micros = req.max_cost.0, "budget reserved");
        Ok(id)
    }
}

fn parse_kind(text: &str) -> Result<TaskKind> {
    match text {
        "default" => Ok(TaskKind::Default),
        "research" => Ok(TaskKind::Research),
        "coding" => Ok(TaskKind::Coding),
        other => Err(PairError::new(
            ErrorCode::Internal,
            format!("unknown task kind {other:?}"),
        )),
    }
}

/// Persist the task's kind on first reserve and return the effective kind. Runs under the
/// advisory lock, so two concurrent first reserves cannot register different kinds.
async fn register_task_kind(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    req: &ReserveRequest,
    inherit_kind: bool,
    now: DateTime<Utc>,
) -> Result<TaskKind> {
    sqlx::query(
        "INSERT INTO budget_tasks (task_id, task_kind, created_at) VALUES ($1, $2, $3) \
         ON CONFLICT (task_id) DO NOTHING",
    )
    .bind(req.task.0)
    .bind(req.kind.as_str())
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(|e| db_err("register task kind", e))?;
    let stored: String =
        sqlx::query_scalar("SELECT task_kind FROM budget_tasks WHERE task_id = $1")
            .bind(req.task.0)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| db_err("read task kind", e))?;
    let stored = parse_kind(&stored)?;
    if !inherit_kind && stored != req.kind {
        return Err(PairError::new(
            ErrorCode::Conflict,
            format!(
                "task {} is a {} task; cannot reserve as {}",
                req.task,
                stored.as_str(),
                req.kind.as_str()
            ),
        ));
    }
    Ok(stored)
}

async fn totals(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: TaskId,
    period: Period,
) -> Result<Totals> {
    let row: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
           COALESCE(SUM(counted_micros) FILTER (WHERE task_id = $1), 0)::BIGINT, \
           COALESCE(SUM(counted_micros) FILTER (WHERE period_day = $2), 0)::BIGINT, \
           COALESCE(SUM(counted_micros) FILTER (WHERE period_month = $3), 0)::BIGINT, \
           COALESCE(SUM(counted_micros) FILTER (WHERE period_month = $3 AND category = 'classifier'), 0)::BIGINT \
         FROM budget_reservations",
    )
    .bind(task.0)
    .bind(period.day)
    .bind(period.month)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| db_err("sum caps", e))?;
    Ok(Totals {
        task: row.0,
        day: row.1,
        month: row.2,
        classifier_month: row.3,
    })
}
