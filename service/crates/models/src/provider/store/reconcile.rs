//! Budget reconciliation for recorded model calls.
//!
//! Lifecycle: `unpriced` -> (`mark_priced`) `priced` -> (`reconcile_call`) `reconciled`.
//! A call only becomes `reconciled` when the budget ledger settled its reservation; an unknown
//! cost leaves both the call and the reservation unresolved (spec 6: never assume zero cost).
use super::{db_err, ConversationStore, CostState};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ModelCallId, ReservationId};
use pair_core::money::Micros;
use pair_core::traits::Budget;
use pair_core::types::{LedgerEntry, UsageReport};

impl ConversationStore {
    /// Attach a computed cost to a call that was recorded without one.
    pub async fn mark_priced(
        &self,
        id: ModelCallId,
        cost: Micros,
        price_version: &str,
    ) -> Result<()> {
        if cost < Micros::ZERO {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "cost must not be negative",
            ));
        }
        let n = sqlx::query(
            "UPDATE model_calls SET cost_micros = $2, price_version = $3, cost_state = 'priced' \
             WHERE id = $1 AND cost_state = 'unpriced'",
        )
        .bind(id.0)
        .bind(cost.0)
        .bind(price_version)
        .execute(&self.pool)
        .await
        .map_err(|e| db_err(&e))?
        .rows_affected();
        if n == 0 {
            // Distinguish a missing call from one already priced.
            self.get_model_call(id).await?;
            return Err(PairError::new(
                ErrorCode::Conflict,
                format!("model call {id} is not unpriced"),
            ));
        }
        Ok(())
    }

    /// Settle the call's reservation in the budget ledger using the call's stored usage, and mark
    /// the call `reconciled` only if the ledger entry is settled. Idempotent via the budget's
    /// reconciliation key.
    pub async fn reconcile_call(
        &self,
        budget: &dyn Budget,
        id: ModelCallId,
        reservation: ReservationId,
    ) -> Result<LedgerEntry> {
        let call = self.get_model_call(id).await?;
        let usage = UsageReport {
            input_tokens: call.input_tokens,
            output_tokens: call.output_tokens,
            actual_cost: call.cost,
            price_version: call.price_version.unwrap_or_default(),
        };
        let entry = budget.reconcile(reservation, usage).await?;
        if entry.settled && call.cost_state != CostState::Reconciled {
            self.mark_reconciled(id, reservation).await?;
        } else if !entry.settled {
            self.link_reservation(id, reservation).await?;
        }
        Ok(entry)
    }

    /// Remember which reservation is still waiting on this call's cost.
    async fn link_reservation(&self, id: ModelCallId, reservation: ReservationId) -> Result<()> {
        sqlx::query("UPDATE model_calls SET reservation_id = $2 WHERE id = $1")
            .bind(id.0)
            .bind(reservation.0)
            .execute(&self.pool)
            .await
            .map_err(|e| db_err(&e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::test_db::{budget_yaml, TestDb};
    use crate::provider::store::ModelCallRecord;
    use pair_core::ids::{TaskId, TraceId};
    use pair_core::types::{DataClass, ModelRequest, ModelResponse};

    const PRICE: &str = "p-test-1";
    const RESERVED_MICROS: i64 = 5_000;
    const ACTUAL_MICROS: i64 = 1_234;

    fn unpriced_call() -> ModelCallRecord {
        let req = ModelRequest {
            model_id: "m".into(),
            messages: Vec::new(),
            max_output_tokens: 16,
            deadline_ms: 1_000,
            data_class: DataClass::Public,
            task: TaskId::new(),
            trace: TraceId::new(),
        };
        let resp = ModelResponse {
            resolved_model: "m-2026".into(),
            text: "hi".into(),
            usage: UsageReport {
                input_tokens: 100,
                output_tokens: 50,
                actual_cost: None,
                price_version: String::new(),
            },
            provider_request_id: None,
            latency_ms: 5,
        };
        ModelCallRecord::from_response(&req, "anthropic", "test", None, &resp)
    }

    async fn reservation_state(db: &TestDb, id: ReservationId) -> (String, i64) {
        sqlx::query_as("SELECT state, counted_micros FROM budget_reservations WHERE id = $1")
            .bind(id.0)
            .fetch_one(&db.pool)
            .await
            .expect("reservation row")
    }

    #[tokio::test]
    async fn test_reconcile_call_priced_call_becomes_reconciled() {
        let db = TestDb::create().await;
        let budget = db.budget(&budget_yaml(2000, 100, 100, 10), PRICE);
        let store = ConversationStore::new(db.pool.clone());
        let call = unpriced_call();
        store.record_model_call(&call).await.expect("record");
        assert_eq!(
            store.get_model_call(call.id).await.expect("get").cost_state,
            CostState::Unpriced
        );
        let reservation = budget
            .reserve(call.task, Micros(RESERVED_MICROS))
            .await
            .expect("reserve");

        store
            .mark_priced(call.id, Micros(ACTUAL_MICROS), PRICE)
            .await
            .expect("price");
        let priced = store.get_model_call(call.id).await.expect("get");
        assert_eq!(priced.cost_state, CostState::Priced);
        assert_eq!(priced.cost, Some(Micros(ACTUAL_MICROS)));

        let entry = store
            .reconcile_call(&budget, call.id, reservation)
            .await
            .expect("reconcile");
        assert!(entry.settled);
        assert_eq!(entry.amount, Micros(ACTUAL_MICROS));
        let done = store.get_model_call(call.id).await.expect("get");
        assert_eq!(done.cost_state, CostState::Reconciled);
        assert_eq!(done.reservation, Some(reservation));
        assert_eq!(
            reservation_state(&db, reservation).await,
            ("settled".into(), ACTUAL_MICROS)
        );

        // Replaying is idempotent: same ledger entry, still reconciled.
        let again = store
            .reconcile_call(&budget, call.id, reservation)
            .await
            .expect("replay");
        assert_eq!(again.id, entry.id);
    }

    #[tokio::test]
    async fn test_reconcile_call_unknown_cost_leaves_call_and_reservation_unresolved() {
        let db = TestDb::create().await;
        let budget = db.budget(&budget_yaml(2000, 100, 100, 10), PRICE);
        let store = ConversationStore::new(db.pool.clone());
        let call = unpriced_call();
        store.record_model_call(&call).await.expect("record");
        let reservation = budget
            .reserve(call.task, Micros(RESERVED_MICROS))
            .await
            .expect("reserve");

        let entry = store
            .reconcile_call(&budget, call.id, reservation)
            .await
            .expect("reconcile with unknown cost is not an error");
        assert!(!entry.settled);
        let after = store.get_model_call(call.id).await.expect("get");
        assert_eq!(after.cost_state, CostState::Unpriced, "not reconciled");
        assert_eq!(after.reservation, Some(reservation), "linked for follow-up");
        assert_eq!(
            reservation_state(&db, reservation).await,
            ("unresolved".into(), RESERVED_MICROS),
            "full reservation stays counted"
        );
    }

    #[tokio::test]
    async fn test_mark_priced_already_priced_conflicts() {
        let db = TestDb::create().await;
        let store = ConversationStore::new(db.pool.clone());
        let call = unpriced_call();
        store.record_model_call(&call).await.expect("record");
        store
            .mark_priced(call.id, Micros(1), PRICE)
            .await
            .expect("first");
        let err = store
            .mark_priced(call.id, Micros(2), PRICE)
            .await
            .expect_err("second");
        assert_eq!(err.code, ErrorCode::Conflict);
        let missing = store
            .mark_priced(ModelCallId::new(), Micros(2), PRICE)
            .await
            .expect_err("missing");
        assert_eq!(missing.code, ErrorCode::NotFound);
    }
}
