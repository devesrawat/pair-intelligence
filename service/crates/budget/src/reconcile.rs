//! Reconciliation: idempotent settlement of a reservation against a usage report.
use crate::ledger::{db_err, PgBudget};
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{LedgerEntryId, ReservationId};
use pair_core::money::Micros;
use pair_core::types::{LedgerEntry, UsageReport};
use sqlx::{Postgres, Transaction};

const STATE_HELD: &str = "held";
const STATE_UNRESOLVED: &str = "unresolved";
const STATE_SETTLED: &str = "settled";

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

fn entry(row: (uuid::Uuid, uuid::Uuid, i64, bool)) -> LedgerEntry {
    LedgerEntry {
        id: LedgerEntryId(row.0),
        reservation: ReservationId(row.1),
        amount: Micros(row.2),
        settled: row.3,
    }
}

impl PgBudget {
    pub(crate) async fn reconcile_at(
        &self,
        id: ReservationId,
        usage: UsageReport,
        now: DateTime<Utc>,
    ) -> Result<LedgerEntry> {
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
        let res: Option<(String, i64)> = sqlx::query_as(
            "SELECT state, reserved_micros FROM budget_reservations WHERE id = $1 FOR UPDATE",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("lock reservation", e))?;
        let (state, reserved) =
            res.ok_or_else(|| PairError::new(ErrorCode::NotFound, "reservation not found"))?;

        if let Some(existing) = find_entry(&mut tx, id, &key).await? {
            return Ok(entry(existing));
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
        if settled && amount > reserved {
            tracing::warn!(reservation = %id, reserved, actual = amount, "actual cost exceeded reservation");
        }
        let ledger_id = LedgerEntryId::new();
        sqlx::query(
            "INSERT INTO budget_ledger \
             (id, reservation_id, reconciliation_key, amount_micros, settled, input_tokens, output_tokens, price_version, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
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
        tracing::info!(reservation = %id, settled, amount, "budget reconciled");
        Ok(LedgerEntry {
            id: ledger_id,
            reservation: id,
            amount: Micros(amount),
            settled,
        })
    }
}

async fn find_entry(
    tx: &mut Transaction<'_, Postgres>,
    id: ReservationId,
    key: &str,
) -> Result<Option<(uuid::Uuid, uuid::Uuid, i64, bool)>> {
    sqlx::query_as(
        "SELECT id, reservation_id, amount_micros, settled FROM budget_ledger \
         WHERE reservation_id = $1 AND reconciliation_key = $2",
    )
    .bind(id.0)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| db_err("find ledger entry", e))
}
