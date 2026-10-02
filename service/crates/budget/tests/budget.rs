#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{yaml, TestDb, PRICE};
use pair_budget::{BudgetConfig, ReserveRequest};
use pair_core::error::ErrorCode;
use pair_core::ids::TaskId;
use pair_core::money::Micros;
use pair_core::traits::Budget;
use pair_core::types::UsageReport;
use std::sync::Arc;

fn usage(cost: Option<i64>) -> UsageReport {
    UsageReport {
        input_tokens: 100,
        output_tokens: 50,
        actual_cost: cost.map(Micros),
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
async fn parallel_reservations_cannot_overspend() {
    let db = TestDb::create().await;
    // Daily cap 1.00 USD; each task reserves 0.03 USD => at most 33 accepted of 50.
    let budget = Arc::new(db.budget(&yaml(2000, 100, 100, 10)));
    let mut handles = Vec::new();
    for _ in 0..50 {
        let b = Arc::clone(&budget);
        handles.push(tokio::spawn(async move {
            b.reserve(TaskId::new(), Micros(30_000)).await
        }));
    }
    let mut accepted = 0i64;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => accepted += 1,
            Err(e) => assert_eq!(e.code, ErrorCode::BudgetExceeded),
        }
    }
    assert_eq!(accepted, 33);
    assert!(accepted * 30_000 <= 1_000_000);
    assert_eq!(counted(&db).await, accepted * 30_000);
}

#[tokio::test]
async fn reconcile_is_idempotent() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 100, 100, 10));
    let id = budget.reserve(TaskId::new(), Micros(50_000)).await.unwrap();
    let a = budget.reconcile(id, usage(Some(20_000))).await.unwrap();
    let b = budget.reconcile(id, usage(Some(20_000))).await.unwrap();
    assert_eq!(a.id, b.id);
    assert!(a.settled);
    assert_eq!(a.amount, Micros(20_000));
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM budget_ledger")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    // Unused remainder released: only actual spend counts.
    assert_eq!(counted(&db).await, 20_000);
    // A different report for a settled reservation is a conflict, not a second settlement.
    let err = budget.reconcile(id, usage(Some(30_000))).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(counted(&db).await, 20_000);
}

#[tokio::test]
async fn unknown_usage_retains_reservation() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 10, 100, 10));
    let id = budget.reserve(TaskId::new(), Micros(80_000)).await.unwrap();
    let entry = budget.reconcile(id, usage(None)).await.unwrap();
    assert!(!entry.settled);
    assert_eq!(entry.amount, Micros(80_000));
    let state: String = sqlx::query_scalar("SELECT state FROM budget_reservations WHERE id = $1")
        .bind(id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "unresolved");
    // Still counted: a further 0.03 would push the 0.10 daily cap over.
    let err = budget
        .reserve(TaskId::new(), Micros(30_000))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BudgetExceeded);
    // Replay is idempotent; a later known cost resolves it and releases the remainder.
    assert_eq!(
        budget.reconcile(id, usage(None)).await.unwrap().id,
        entry.id
    );
    let settled = budget.reconcile(id, usage(Some(10_000))).await.unwrap();
    assert!(settled.settled);
    assert_eq!(counted(&db).await, 10_000);
    budget.reserve(TaskId::new(), Micros(30_000)).await.unwrap();
}

#[tokio::test]
async fn unknown_price_denies() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 100, 100, 10));
    let req = ReserveRequest::metered(
        TaskId::new(),
        Micros(1_000),
        pair_budget::TaskKind::Default,
        "nope".into(),
    );
    assert_eq!(
        budget.reserve_with(req).await.unwrap_err().code,
        ErrorCode::BudgetUnknownPrice
    );
    assert_eq!(counted(&db).await, 0);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM budget_reservations")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);

    // No current price configured: the plain trait entry point refuses too.
    let no_price = pair_budget::PgBudget::new(
        db.pool.clone(),
        BudgetConfig::from_yaml(&yaml(2000, 100, 100, 10)).unwrap(),
        pair_budget::PriceBook::default(),
    );
    let err = no_price
        .reserve(TaskId::new(), Micros(1_000))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BudgetUnknownPrice);

    // Reconciling with an unknown price version changes nothing.
    let id = budget.reserve(TaskId::new(), Micros(5_000)).await.unwrap();
    let mut u = usage(Some(1_000));
    u.price_version = "nope".into();
    assert_eq!(
        budget.reconcile(id, u).await.unwrap_err().code,
        ErrorCode::BudgetUnknownPrice
    );
    assert_eq!(counted(&db).await, 5_000);
}

#[tokio::test]
async fn task_cap_enforced() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 1000, 100, 10)); // task cap 0.10
    let task = TaskId::new();
    budget.reserve(task, Micros(60_000)).await.unwrap();
    let err = budget.reserve(task, Micros(60_000)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::BudgetExceeded);
    budget.reserve(task, Micros(40_000)).await.unwrap();
    // Another task is unaffected.
    budget
        .reserve(TaskId::new(), Micros(100_000))
        .await
        .unwrap();
}

#[tokio::test]
async fn monthly_cap_enforced() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(10, 1000, 100, 10)); // month cap 0.10
    budget
        .reserve(TaskId::new(), Micros(100_000))
        .await
        .unwrap();
    let err = budget.reserve(TaskId::new(), Micros(1)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::BudgetExceeded);
}

#[tokio::test]
async fn classifier_subcap_separate() {
    let db = TestDb::create().await;
    // classifier sub-cap 0.05 inside month 20.00 / day 10.00.
    let budget = db.budget(&yaml(2000, 1000, 5, 100));
    let cls = |c: i64| ReserveRequest::classifier(TaskId::new(), Micros(c), PRICE.to_owned());
    budget.reserve_with(cls(30_000)).await.unwrap();
    budget.reserve_with(cls(20_000)).await.unwrap();
    assert_eq!(
        budget.reserve_with(cls(1)).await.unwrap_err().code,
        ErrorCode::BudgetExceeded
    );
    // Metered spend is not blocked by an exhausted classifier sub-cap...
    budget
        .reserve(TaskId::new(), Micros(500_000))
        .await
        .unwrap();
    // ...and that metered spend did not change the classifier sub-cap outcome.
    assert_eq!(
        budget.reserve_with(cls(1)).await.unwrap_err().code,
        ErrorCode::BudgetExceeded
    );
}

#[test]
fn auto_top_up_cannot_be_enabled() {
    let on = yaml(2000, 100, 100, 10).replace("auto_top_up: false", "auto_top_up: true");
    assert_eq!(
        BudgetConfig::from_yaml(&on).unwrap_err().code,
        ErrorCode::InvalidInput
    );
    // The shipped config parses, with exact integer micros.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/budget.yaml");
    let cfg = BudgetConfig::load(path).unwrap();
    assert_eq!(cfg.monthly_cap(), Micros(20_000_000));
    assert_eq!(cfg.daily_cap(), Micros(1_000_000));
    assert_eq!(cfg.classifier_monthly_subcap(), Micros(1_000_000));
    assert_eq!(
        cfg.task_cap(pair_budget::TaskKind::Default),
        Micros(100_000)
    );
}
