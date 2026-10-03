#![allow(clippy::unwrap_used)]
//! The sweeper's requeue: a retry limit for runs that keep being interrupted, and operator
//! interrupts that it must not undo.
mod common;

use common::{expire_leases, steady_cfg, TestDb};
use pair_core::ids::{IdempotencyKey, RunId};
use pair_core::traits::Workflows;
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{JobStore, MAX_INTERRUPTS};
use serde_json::json;

fn input() -> WorkflowInput {
    WorkflowInput {
        kind: "t".to_owned(),
        payload: json!({"n": 1}),
    }
}

/// One crash cycle: a worker claims the run and dies, the lease lapses, the sweeper runs.
async fn crash_and_sweep(db: &TestDb, store: &JobStore) {
    assert!(store.claim("ghost").await.unwrap().is_some(), "claimable");
    expire_leases(db).await;
    assert_eq!(store.sweep_expired().await.unwrap(), 1);
}

async fn interrupt_count(db: &TestDb, id: RunId) -> i32 {
    sqlx::query_scalar("SELECT interrupt_count FROM workflow_runs WHERE id = $1")
        .bind(id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn requeue_fails_a_run_that_keeps_being_interrupted() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let id = store.start(input(), IdempotencyKey::new()).await.unwrap();
    for cycle in 1..MAX_INTERRUPTS {
        crash_and_sweep(&db, &store).await;
        assert_eq!(store.requeue_interrupted().await.unwrap(), 1, "{cycle}");
        assert_eq!(store.state(id).await.unwrap(), RunState::Queued);
    }
    crash_and_sweep(&db, &store).await;
    assert_eq!(
        store.requeue_interrupted().await.unwrap(),
        0,
        "the last interruption is not requeued"
    );
    assert_eq!(store.state(id).await.unwrap(), RunState::Failed);
    let (failure, finished): (String, bool) =
        sqlx::query_as("SELECT failure, finished_at IS NOT NULL FROM workflow_runs WHERE id = $1")
            .bind(id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        failure.contains(&format!("interrupted {MAX_INTERRUPTS} times")),
        "{failure}"
    );
    assert!(finished);
    assert_eq!(
        interrupt_count(&db, id).await,
        i32::try_from(MAX_INTERRUPTS).unwrap()
    );
    // Nothing left to claim, and a further sweep changes nothing.
    assert!(store.claim("w").await.unwrap().is_none());
    assert_eq!(store.requeue_interrupted().await.unwrap(), 0);
}

#[tokio::test]
async fn operator_interrupt_is_not_requeued_by_the_sweeper() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let id = store.start(input(), IdempotencyKey::new()).await.unwrap();
    assert!(store.claim("w1").await.unwrap().is_some());
    assert!(store.operator_interrupt(id).await.unwrap());
    assert_eq!(store.state(id).await.unwrap(), RunState::Interrupted);

    assert_eq!(
        store.requeue_interrupted().await.unwrap(),
        0,
        "an operator's interrupt stands"
    );
    assert_eq!(store.state(id).await.unwrap(), RunState::Interrupted);
    assert_eq!(interrupt_count(&db, id).await, 0, "not counted as a crash");

    // An explicit resume is the operator's way back; the marker must not shield the run later.
    assert_eq!(store.resume(id).await.unwrap(), RunState::Queued);
    crash_and_sweep(&db, &store).await;
    assert_eq!(
        store.requeue_interrupted().await.unwrap(),
        1,
        "a later lease lapse is ordinary recovery again"
    );
    assert_eq!(interrupt_count(&db, id).await, 1);
}

#[tokio::test]
async fn operator_interrupt_ignores_runs_that_are_not_running() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let id = store.start(input(), IdempotencyKey::new()).await.unwrap();
    assert!(
        !store.operator_interrupt(id).await.unwrap(),
        "a queued run is not interrupted"
    );
    assert_eq!(store.state(id).await.unwrap(), RunState::Queued);
}
