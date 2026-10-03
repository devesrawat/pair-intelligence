//! Reconciliation: idempotent settlement of a reservation against a usage report.
use crate::ledger::{db_err, PgBudget};
use crate::reserve::RESERVE_LOCK_KEY;
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{LedgerEntryId, ReservationId};
use pair_core::money::Micros;
use pair_core::types::{LedgerEntry, UsageReport};
use sqlx::{Postgres, Transaction};

const STATE_HELD: &str = "held";
const STATE_UNRESOLVED: &str = "unresolved";
const STATE_SETTLED: &str = "settled";

/// Result of a reconciliation: the ledger entry plus whether the actual cost exceeded what was
/// reserved. An overrun is already counted (the cap checks of later reserves see the real spend)
/// but the call that caused it was not blocked, so callers should alert on it.
#[derive(Debug, Clone)]
pub struct Reconciled {
    pub entry: LedgerEntry,
    /// `actual > reserved` for a settled entry.
    pub overrun: bool,
}

type LedgerRow = (uuid::Uuid, uuid::Uuid, i64, bool, bool);

/// Canonical identity of a usage report; an identical replay maps to the same ledger row.
fn reconciliation_key(u: &UsageReport) -> String {
    let cost = u
        .actual_cost
        .map_or_else(|| "unknown".to_owned(), |c| c.0.to_string());
    format!(
        "{}|{}|{}|{}",
        u.input_tokens, u.output_tokens, cost, u.price_version
    )
}

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v)
        .map_err(|_| PairError::new(ErrorCode::InvalidInput, format!("{what} out of range")))
}

fn reconciled(row: LedgerRow) -> Reconciled {
    Reconciled {
        entry: LedgerEntry {
            id: LedgerEntryId(row.0),
            reservation: ReservationId(row.1),
            amount: Micros(row.2),
            settled: row.3,
        },
        overrun: row.4,
    }
}

impl PgBudget {
    /// Settle a reservation and report whether the actual cost overran it. Same semantics as
    /// `Budget::reconcile`, which discards the overrun flag.
    pub async fn reconcile_detailed(
        &self,
        id: ReservationId,
        usage: UsageReport,
    ) -> Result<Reconciled> {
        self.reconcile_at(id, usage, Utc::now()).await
    }

    pub(crate) async fn reconcile_at(
        &self,
        id: ReservationId,
        usage: UsageReport,
        now: DateTime<Utc>,
    ) -> Result<Reconciled> {
        if let Some(cost) = usage.actual_cost {
            if cost < Micros::ZERO {
                return Err(PairError::new(
                    ErrorCode::InvalidInput,
                    "actual_cost must not be negative",
                ));
            }
            if !self.prices.is_known(&usage.price_version) {
                return Err(PairError::new(
                    ErrorCode::BudgetUnknownPrice,
                    format!("unknown price version {:?}", usage.price_version),
                ));
            }
        }
        let input = to_i64(usage.input_tokens, "input_tokens")?;
        let output = to_i64(usage.output_tokens, "output_tokens")?;
        let key = reconciliation_key(&usage);

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| db_err("begin reconcile", e))?;
        // Same lock as reserve: a settlement that raises counted spend must not interleave with a
        // reserve that already checked the caps against the old total.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(RESERVE_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_err("advisory lock", e))?;
        let res: Option<(String, i64, String)> = sqlx::query_as(
            "SELECT state, reserved_micros, price_version FROM budget_reservations WHERE id = $1 FOR UPDATE",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("lock reservation", e))?;
        let (state, reserved, reserved_version) =
            res.ok_or_else(|| PairError::new(ErrorCode::NotFound, "reservation not found"))?;

        if let Some(existing) = find_entry(&mut tx, id, &key).await? {
            // A late replay of an earlier unresolved report must see the settlement, not the
            // stale unresolved row it originally produced.
            if !existing.3 && state == STATE_SETTLED {
                if let Some(settled) = find_settled(&mut tx, id).await? {
                    return Ok(reconciled(settled));
                }
            }
            return Ok(reconciled(existing));
        }
        if state == STATE_SETTLED {
            return Err(PairError::new(
                ErrorCode::Conflict,
                "reservation already settled with a different usage report",
            ));
        }
        debug_assert!(state == STATE_HELD || state == STATE_UNRESOLVED);

        // Unknown actual cost: keep the full reservation counted; never assume zero.
        let (settled, amount, new_state) = match usage.actual_cost {
            Some(c) => (true, c.0, STATE_SETTLED),
            None => (false, reserved, STATE_UNRESOLVED),
        };
        if settled && usage.price_version != reserved_version {
            return Err(PairError::new(
                ErrorCode::Conflict,
                format!(
                    "settlement price version {:?} differs from reservation price version {reserved_version:?}",
                    usage.price_version
                ),
            ));
        }
        let overrun = settled && amount > reserved;
        if overrun {
            tracing::warn!(reservation = %id, reserved, actual = amount, "actual cost exceeded reservation");
        }
        let ledger_id = LedgerEntryId::new();
        sqlx::query(
            "INSERT INTO budget_ledger \
             (id, reservation_id, reconciliation_key, amount_micros, settled, input_tokens, output_tokens, price_version, created_at, overrun) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(ledger_id.0)
        .bind(id.0)
        .bind(&key)
        .bind(amount)
        .bind(settled)
        .bind(input)
        .bind(output)
        .bind(&usage.price_version)
        .bind(now)
        .bind(overrun)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_err("insert ledger", e))?;

        // Settled: count actual spend, releasing any unused remainder. Unresolved: count reservation.
        sqlx::query(
            "UPDATE budget_reservations SET state = $2, counted_micros = $3, \
             resolved_at = CASE WHEN $2 = 'settled' THEN $4 ELSE resolved_at END WHERE id = $1",
        )
        .bind(id.0)
        .bind(new_state)
        .bind(amount)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_err("update reservation", e))?;
        tx.commit()
            .await
            .map_err(|e| db_err("commit reconcile", e))?;
        tracing::info!(reservation = %id, settled, amount, overrun, "budget reconciled");
        Ok(Reconciled {
            entry: LedgerEntry {
                id: ledger_id,
                reservation: id,
                amount: Micros(amount),
                settled,
            },
            overrun,
        })
    }
}

async fn find_entry(
    tx: &mut Transaction<'_, Postgres>,
    id: ReservationId,
    key: &str,
) -> Result<Option<LedgerRow>> {
    sqlx::query_as(
        "SELECT id, reservation_id, amount_micros, settled, overrun FROM budget_ledger \
         WHERE reservation_id = $1 AND reconciliation_key = $2",
    )
    .bind(id.0)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| db_err("find ledger entry", e))
}

async fn find_settled(
    tx: &mut Transaction<'_, Postgres>,
    id: ReservationId,
) -> Result<Option<LedgerRow>> {
    sqlx::query_as(
        "SELECT id, reservation_id, amount_micros, settled, overrun FROM budget_ledger \
         WHERE reservation_id = $1 AND settled",
    )
    .bind(id.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| db_err("find settled entry", e))
}
