#![allow(clippy::unwrap_used)]
//! C1: a worker that outlives its lease must never run an effect twice. The first worker stays
//! ALIVE in every test (no abort()) and the assertions are on observable effect counts.
mod common;

use async_trait::async_trait;
use common::{fast_cfg, TestDb};
use pair_core::ids::{IdempotencyKey, RunId};
use pair_core::traits::Workflows;
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{
    EffectError, JobStore, Reconciliation, StepCtx, StepError, StepHandler, StepOutcome, Worker,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;

const SLOW_EXEC: Duration = Duration::from_millis(2000);

fn input() -> WorkflowInput {
    WorkflowInput {
        kind: "fx".to_owned(),
        payload: json!({"n": 1}),
    }
}

async fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..2000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

/// Simulates a paused/partitioned worker: its lease is already expired as far as the DB is concerned.
async fn expire_lease(db: &TestDb, id: RunId) {
    sqlx::query(
        "UPDATE workflow_runs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(id.0)
    .execute(&db.pool)
    .await
    .unwrap();
}

async fn intent_rows(db: &TestDb) -> Vec<(String, String)> {
    sqlx::query_as("SELECT effect_key, status FROM effect_intents")
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

/// The remote effect "lands" only when the exec future finishes its slow I/O, like a real send.
/// The reconcile callback answers truthfully from what has landed so far.
#[derive(Default)]
struct SlowEffect {
    landed: AtomicU32,
    execs: AtomicU32,
    exec_started: AtomicBool,
}

#[async_trait]
impl StepHandler for SlowEffect {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let out = ctx
            .effect(
                "send",
                json!({"to": "a@b.c"}),
                || async {
                    self.execs.fetch_add(1, SeqCst);
                    self.exec_started.store(true, SeqCst);
                    tokio::time::sleep(SLOW_EXEC).await;
                    self.landed.fetch_add(1, SeqCst);
                    Ok(json!({"message_id": "m-1"}))
                },
                |_| async {
                    Ok(if self.landed.load(SeqCst) > 0 {
                        Reconciliation::Applied(json!({"message_id": "m-1"}))
                    } else {
                        Reconciliation::NotApplied
                    })
                },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

async fn started(store: &JobStore) -> RunId {
    store.start(input(), IdempotencyKey::new()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_worker_after_lease_expiry_does_not_repeat_effect() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg()); // lease_ttl 600ms; the exec below takes 2s
    let handler = Arc::new(SlowEffect::default());
    let id = started(&store).await;

    let w1 = Worker::new(store.clone(), "w1").with_handler("fx", handler.clone());
    let first = tokio::spawn(async move { w1.run_once().await });
    wait_until(|| handler.exec_started.load(SeqCst)).await;

    // Well past the 600ms lease TTL while the effect is still in flight.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        store.sweep_expired().await.unwrap(),
        0,
        "a live worker's lease must be renewed while its step executes"
    );
    store.resume(id).await.unwrap();
    let w2 = Worker::new(store.clone(), "w2").with_handler("fx", handler.clone());
    assert_eq!(
        w2.run_once().await.unwrap(),
        None,
        "run must not be claimable"
    );

    assert_eq!(
        first.await.unwrap().unwrap(),
        Some((id, RunState::Succeeded))
    );
    assert_eq!(handler.execs.load(SeqCst), 1);
    assert_eq!(handler.landed.load(SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_lease_cancels_inflight_step_so_successor_runs_effect_once() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let handler = Arc::new(SlowEffect::default());
    let id = started(&store).await;

    let w1 = Worker::new(store.clone(), "w1").with_handler("fx", handler.clone());
    let first = tokio::spawn(async move { w1.run_once().await });
    wait_until(|| handler.exec_started.load(SeqCst)).await;

    expire_lease(&db, id).await;
    store.resume(id).await.unwrap();
    let w2 = Worker::new(store.clone(), "w2").with_handler("fx", handler.clone());
    let second = w2.run_once().await.unwrap();

    // w1 is still alive; it must notice the loss, drop its in-flight step and abandon the run.
    first.await.unwrap().unwrap();
    assert_eq!(second, Some((id, RunState::Succeeded)));
    assert_eq!(
        handler.landed.load(SeqCst),
        1,
        "effect landed more than once"
    );
}

/// First exec blocks its thread (cannot be cancelled) and then reports a definite failure; the
/// second worker has meanwhile taken over and is mid-effect. The late Failed must not delete the
/// successor's intent.
struct FailLate {
    calls: AtomicU32,
    landed: AtomicU32,
    second_in_exec: AtomicBool,
}

#[async_trait]
impl StepHandler for FailLate {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let out = ctx
            .effect(
                "send",
                json!({"to": "a@b.c"}),
                || async {
                    if self.calls.fetch_add(1, SeqCst) == 0 {
                        std::thread::sleep(Duration::from_millis(1500));
                        return Err(EffectError::Failed("late failure".into()));
                    }
                    self.second_in_exec.store(true, SeqCst);
                    tokio::time::sleep(SLOW_EXEC).await;
                    self.landed.fetch_add(1, SeqCst);
                    Ok(json!({"message_id": "m-2"}))
                },
                |_| async { Ok(Reconciliation::NotApplied) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_delete_does_not_remove_other_workers_intent() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let handler = Arc::new(FailLate {
        calls: AtomicU32::new(0),
        landed: AtomicU32::new(0),
        second_in_exec: AtomicBool::new(false),
    });
    let id = started(&store).await;

    let w1 = Worker::new(store.clone(), "w1").with_handler("fx", handler.clone());
    let first = tokio::spawn(async move { w1.run_once().await });
    wait_until(|| handler.calls.load(SeqCst) == 1).await;

    expire_lease(&db, id).await;
    store.resume(id).await.unwrap();
    let w2 = Worker::new(store.clone(), "w2").with_handler("fx", handler.clone());
    let second = tokio::spawn(async move { w2.run_once().await });
    wait_until(|| handler.second_in_exec.load(SeqCst)).await;

    // w1 wakes up from its blocking exec with a stale Failed while w2 is mid-effect.
    first.await.unwrap().unwrap();
    let rows = intent_rows(&db).await;
    assert_eq!(rows.len(), 1, "other worker's intent was deleted: {rows:?}");
    assert_eq!(rows[0].1, "executing");

    assert_eq!(
        second.await.unwrap().unwrap(),
        Some((id, RunState::Succeeded))
    );
    assert_eq!(handler.landed.load(SeqCst), 1);
    assert_eq!(intent_rows(&db).await[0].1, "completed");
}

struct LateEffect {
    fired: AtomicU32,
    reached: AtomicBool,
}

#[async_trait]
impl StepHandler for LateEffect {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        self.reached.store(true, SeqCst);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let out = ctx
            .effect(
                "send",
                json!({"to": "a@b.c"}),
                || async {
                    self.fired.fetch_add(1, SeqCst);
                    Ok(json!({"ok": true}))
                },
                |_| async { Ok(Reconciliation::NotApplied) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_run_does_not_fire_inflight_effect() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let handler = Arc::new(LateEffect {
        fired: AtomicU32::new(0),
        reached: AtomicBool::new(false),
    });
    let id = started(&store).await;

    let w1 = Worker::new(store.clone(), "w1").with_handler("fx", handler.clone());
    let first = tokio::spawn(async move { w1.run_once().await });
    wait_until(|| handler.reached.load(SeqCst)).await;
    assert_eq!(store.cancel(id).await.unwrap(), RunState::Cancelled);

    first.await.unwrap().unwrap();
    assert_eq!(handler.fired.load(SeqCst), 0, "effect fired after cancel");
    assert_eq!(store.state(id).await.unwrap(), RunState::Cancelled);
    assert!(intent_rows(&db).await.is_empty());
}
