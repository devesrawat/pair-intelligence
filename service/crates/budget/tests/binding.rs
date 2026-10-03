#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Adapter-route review H2/H3: the hard ceiling on one amount and the reservation binding.
mod common;

use common::{yaml, TestDb, PRICE};
use pair_budget::{ReserveRequest, TaskKind, MAX_COST_MICROS};
use pair_core::error::ErrorCode;
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::Micros;
use pair_core::traits::Budget;
use pair_core::types::UsageReport;

fn usage(cost: i64) -> UsageReport {
    UsageReport {
        input_tokens: 10,
        output_tokens: 5,
        actual_cost: Some(Micros(cost)),
        price_version: PRICE.to_owned(),
    }
}

async fn counted(db: &TestDb) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(SUM(counted_micros), 0)::BIGINT FROM budget_reservations")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn hard_ceiling_refuses_absurd_reserve_and_settle_and_budget_still_works() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 1000, 100, 10));
    let id = budget.reserve(TaskId::new(), Micros(10_000)).await.unwrap();
    for cost in [i64::MAX, MAX_COST_MICROS + 1] {
        let err = budget.reconcile(id, usage(cost)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput, "{cost}");
    }
    let err = budget
        .reserve(TaskId::new(), Micros(i64::MAX))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    assert_eq!(counted(&db).await, 10_000, "nothing changed");
    // The SUM the cap checks read is intact, so later reserves and settlements work.
    budget.reserve(TaskId::new(), Micros(10_000)).await.unwrap();
    budget.reconcile(id, usage(500)).await.unwrap();
}

#[tokio::test]
async fn reservation_binding_reports_task_model_and_hold() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 1000, 100, 10));
    let task = TaskId::new();
    let req = ReserveRequest::metered(task, Micros(7_000), TaskKind::Default, PRICE.to_owned());
    let id = budget
        .reserve_for_model(req, Some("model-x"))
        .await
        .unwrap();
    let binding = budget.reservation_binding(id).await.unwrap();
    assert_eq!(binding.task, task);
    assert_eq!(binding.model_id.as_deref(), Some("model-x"));
    assert_eq!(binding.price_version, PRICE);
    assert_eq!(binding.reserved, Micros(7_000));

    let plain = budget.reserve(TaskId::new(), Micros(1_000)).await.unwrap();
    let plain = budget.reservation_binding(plain).await.unwrap();
    assert_eq!(plain.model_id, None);

    let err = budget
        .reservation_binding(ReservationId::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}
