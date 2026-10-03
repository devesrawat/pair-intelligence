#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Review findings M1 (reconcile lock + overrun), M2 (task kind) and the LOW items.
mod common;

use common::{yaml, TestDb, PRICE};
use pair_budget::{BudgetConfig, PgBudget, PriceBook, ReserveRequest, TaskKind, RESERVE_LOCK_KEY};
use pair_core::error::ErrorCode;
use pair_core::ids::TaskId;
use pair_core::money::Micros;
use pair_core::traits::Budget;
use pair_core::types::UsageReport;
use std::time::Duration;

fn usage(cost: Option<i64>, version: &str) -> UsageReport {
    UsageReport {
        input_tokens: 10,
        output_tokens: 5,
        actual_cost: cost.map(Micros),
        price_version: version.to_owned(),
    }
}

fn metered(task: TaskId, micros: i64, kind: TaskKind) -> ReserveRequest {
    ReserveRequest::metered(task, Micros(micros), kind, PRICE.to_owned())
}

async fn counted(db: &TestDb) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(SUM(counted_micros), 0)::BIGINT FROM budget_reservations")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

// ---------- M2 ----------

#[tokio::test]
async fn task_cap_follows_task_kind() {
    let db = TestDb::create().await;
    // default 0.10, research/coding 1.00, day 10.00, month 20.00
    let cfg = yaml(2000, 1000, 100, 10).replace("coding_task_cap: 0.10", "coding_task_cap: 1.00");
    let budget = db.budget(&cfg);

    // Registered as coding: the plain trait `reserve` must follow the task's kind, not apply 0.10.
    let coding = TaskId::new();
    budget
        .reserve_with(metered(coding, 60_000, TaskKind::Coding))
        .await
        .unwrap();
    budget.reserve(coding, Micros(60_000)).await.unwrap();
    assert_eq!(counted(&db).await, 120_000);

    // The kind cannot change after the first reserve (cap dodging).
    let err = budget
        .reserve_with(metered(coding, 1_000, TaskKind::Default))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    let other = TaskId::new();
    budget
        .reserve_with(metered(other, 1_000, TaskKind::Research))
        .await
        .unwrap();
    let err = budget
        .reserve_with(metered(other, 1_000, TaskKind::Coding))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    // An unregistered task reserved through the plain trait is a default task: 0.10 cap applies.
    let plain = TaskId::new();
    assert_eq!(
        budget
            .reserve(plain, Micros(150_000))
            .await
            .unwrap_err()
            .code,
        ErrorCode::BudgetExceeded
    );
    // A rejected reserve must not register the kind either.
    budget
        .reserve_with(metered(plain, 150_000, TaskKind::Coding))
        .await
        .unwrap();
}

// ---------- M1 ----------

#[tokio::test]
async fn reconcile_flags_overrun() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 1000, 100, 10));
    let within = budget.reserve(TaskId::new(), Micros(10_000)).await.unwrap();
    let r = budget
        .reconcile_detailed(within, usage(Some(9_000), PRICE))
        .await
        .unwrap();
    assert!(r.entry.settled && !r.overrun);

    let over = budget.reserve(TaskId::new(), Micros(10_000)).await.unwrap();
    let r = budget
        .reconcile_detailed(over, usage(Some(15_000), PRICE))
        .await
        .unwrap();
    assert!(r.entry.settled && r.overrun);
    assert_eq!(r.entry.amount, Micros(15_000));
    assert_eq!(counted(&db).await, 9_000 + 15_000, "overrun is counted");
    // Replays keep reporting it.
    let again = budget
        .reconcile_detailed(over, usage(Some(15_000), PRICE))
        .await
        .unwrap();
    assert!(again.overrun);
}

#[tokio::test]
async fn reconcile_waits_for_the_reserve_lock() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 1000, 100, 10));
    let id = budget.reserve(TaskId::new(), Micros(10_000)).await.unwrap();

    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESERVE_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    let b = budget.clone();
    let task = tokio::spawn(async move { b.reconcile(id, usage(Some(12_000), PRICE)).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !task.is_finished(),
        "reconcile must serialize with reserve on the advisory lock"
    );
    holder.commit().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(counted(&db).await, 12_000);
}

// ---------- LOW: settlement ----------

#[tokio::test]
async fn settlement_price_version_mismatch_is_rejected() {
    let db = TestDb::create().await;
    let cfg = BudgetConfig::from_yaml(&yaml(2000, 1000, 100, 10)).unwrap();
    let budget = PgBudget::new(
        db.pool.clone(),
        cfg,
        PriceBook::new(Some(PRICE.to_owned()), ["p-other".to_owned()]),
    );
    let id = budget.reserve(TaskId::new(), Micros(10_000)).await.unwrap();
    let err = budget
        .reconcile(id, usage(Some(1_000), "p-other"))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(counted(&db).await, 10_000, "mismatch must not settle");
    budget
        .reconcile(id, usage(Some(1_000), PRICE))
        .await
        .unwrap();
}

#[tokio::test]
async fn stale_unresolved_replay_after_settle_returns_settled_entry() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 1000, 100, 10));
    let id = budget.reserve(TaskId::new(), Micros(10_000)).await.unwrap();
    let unresolved = budget.reconcile(id, usage(None, PRICE)).await.unwrap();
    assert!(!unresolved.settled);
    let settled = budget
        .reconcile(id, usage(Some(4_000), PRICE))
        .await
        .unwrap();
    assert!(settled.settled);

    let replay = budget.reconcile(id, usage(None, PRICE)).await.unwrap();
    assert!(replay.settled, "late replay must not look unresolved");
    assert_eq!(replay.id, settled.id);
    assert_eq!(replay.amount, Micros(4_000));
    assert_eq!(counted(&db).await, 4_000);
}

// ---------- LOW: config ----------

fn with(replace_from: &str, replace_to: &str) -> String {
    yaml(2000, 1000, 100, 10).replace(replace_from, replace_to)
}

#[test]
fn config_rejects_cap_ordering_violations() {
    let ok = yaml(2000, 1000, 100, 10);
    BudgetConfig::from_yaml_strict(&ok).unwrap();
    for bad in [
        yaml(2000, 5, 100, 10),     // task cap 0.10 > daily 0.05
        yaml(500, 1000, 100, 10),   // daily 10.00 > monthly 5.00
        yaml(2000, 1000, 2500, 10), // classifier 25.00 > monthly 20.00
        with("coding_task_cap: 0.10", "coding_task_cap: 11.00"), // coding > daily
    ] {
        assert_eq!(
            BudgetConfig::from_yaml_strict(&bad).unwrap_err().code,
            ErrorCode::InvalidInput,
            "{bad}"
        );
    }
}

#[test]
fn config_rejects_unknown_keys() {
    for bad in [
        with("auto_top_up: false", "auto_top_ups: true"),
        with(
            "auto_top_up: false",
            "auto_top_up: false\n  metered_weekly_cap: 5.00",
        ),
        with("schedule:", "surprise: 1\nschedule:"),
        with(
            "timezone: Asia/Kolkata",
            "timezone: Asia/Kolkata\n  tz_offset: 5",
        ),
    ] {
        assert_eq!(
            BudgetConfig::from_yaml(&bad).unwrap_err().code,
            ErrorCode::InvalidInput,
            "{bad}"
        );
    }
    // The shipped config (with its execution section and schedule times) still loads strictly.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/budget.yaml");
    BudgetConfig::load(path).unwrap();
}

#[tokio::test]
async fn test_subscription_floor_reservation_settles_at_zero() {
    let db = TestDb::create().await;
    let budget = db.budget(&yaml(2000, 1000, 100, 10));
    let id = budget
        .reserve_with(metered(TaskId::new(), 1, TaskKind::Default))
        .await
        .unwrap();
    let r = budget
        .reconcile_detailed(id, usage(Some(0), PRICE))
        .await
        .unwrap();
    assert!(r.entry.settled && !r.overrun);
    assert_eq!(r.entry.amount, Micros(0));
    assert_eq!(counted(&db).await, 0, "a zero-cost settle counts nothing");
}
