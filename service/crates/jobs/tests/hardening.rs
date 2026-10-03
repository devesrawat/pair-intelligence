#![allow(clippy::unwrap_used)]
//! Review findings H1, H2, M4, M5 and the run-class idempotency LOW.
mod common;

use async_trait::async_trait;
use chrono::Utc;
use common::{fast_cfg, steady_cfg, TestDb};
use pair_core::error::{ErrorCode, PairError};
use pair_core::ids::{IdempotencyKey, RunId};
use pair_core::traits::{Approvals, Workflows};
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{
    action_hash, EffectError, Intent, IntentReconciler, JobConfig, JobStore, OrphanSweep,
    PgApprovals, Reconciliation, RunClass, StepCtx, StepError, StepHandler, StepOutcome, Worker,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn input(kind: &str) -> WorkflowInput {
    WorkflowInput {
        kind: kind.to_owned(),
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

async fn failure_of(db: &TestDb, id: RunId) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT failure FROM workflow_runs WHERE id = $1")
        .bind(id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .unwrap_or_default()
}

/// Start a run of `kind`, run it to its approval park, grant `payload_for_approval`, return worker + id.
async fn parked_and_granted(
    db: &TestDb,
    store: &JobStore,
    handler: Arc<dyn StepHandler>,
    approved_payload: &Value,
    expiry_in: chrono::Duration,
) -> (Worker, RunId, pair_core::ids::ApprovalId) {
    let id = store
        .start(input("ap"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("ap", handler);
    assert_eq!(
        w.run_once().await.unwrap(),
        Some((id, RunState::WaitingApproval))
    );
    let ap = PgApprovals::new(db.pool.clone())
        .approve(
            &action_hash(approved_payload).unwrap(),
            "devesh",
            Utc::now() + expiry_in,
        )
        .await
        .unwrap();
    store.grant(id, ap).await.unwrap();
    (w, id, ap)
}

fn in_24h() -> chrono::Duration {
    chrono::Duration::hours(23)
}

// ---------- H1: re-entrant consume checks expiry ----------

struct Reenter {
    execs: AtomicU32,
    payload: Value,
    approval_ms: Duration,
}

#[async_trait]
impl StepHandler for Reenter {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        if ctx.run.approval_id.is_none() {
            return Ok(StepOutcome::AwaitApproval {
                action_hash: action_hash(&self.payload)?,
            });
        }
        let out = ctx
            .approved_effect(
                "push",
                self.payload.clone(),
                || async {
                    self.execs.fetch_add(1, SeqCst);
                    tokio::time::sleep(self.approval_ms).await;
                    Err(EffectError::Ambiguous("timeout".into()))
                },
                |_| async { Ok(Reconciliation::NotApplied) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "push".into(),
            output: out,
        })
    }
}

#[tokio::test]
async fn reentrant_consume_rejects_expired_approval() {
    let db = TestDb::new().await;
    let mut cfg = steady_cfg();
    cfg.retry.max_attempts = 3;
    cfg.retry.base = Duration::from_millis(10);
    let store = db.store(cfg);
    let handler = Arc::new(Reenter {
        execs: AtomicU32::new(0),
        payload: json!({"branch": "x"}),
        approval_ms: Duration::from_millis(2800),
    });
    // The approval lives 2s, counted from when it is created; the first attempt's effect outlasts
    // it and ends ambiguously, so the retry re-enters the (already consumed) approval after expiry.
    let (w, id, _) = parked_and_granted(
        &db,
        &store,
        handler.clone(),
        &handler.payload,
        chrono::Duration::milliseconds(2000),
    )
    .await;
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
    assert_eq!(handler.execs.load(SeqCst), 1);
    assert!(
        failure_of(&db, id).await.contains("ApprovalExpired"),
        "expected expiry failure"
    );
}

// ---------- H2: approval bound to payload and to one effect ----------

struct TwoEffects {
    approved: Value,
    first: Value,
    second: Value,
    second_key: &'static str,
    execs: AtomicU32,
    second_err: Mutex<Option<ErrorCode>>,
}

impl TwoEffects {
    fn new(approved: Value, first: Value, second: Value, second_key: &'static str) -> Arc<Self> {
        Arc::new(Self {
            approved,
            first,
            second,
            second_key,
            execs: AtomicU32::new(0),
            second_err: Mutex::new(None),
        })
    }

    async fn fire(&self, ctx: &StepCtx, key: &str, payload: &Value) -> Result<Value, StepError> {
        ctx.approved_effect(
            key,
            payload.clone(),
            || async {
                self.execs.fetch_add(1, SeqCst);
                Ok(json!({"ok": key}))
            },
            |_| async { Ok(Reconciliation::NotApplied) },
        )
        .await
    }
}

#[async_trait]
impl StepHandler for TwoEffects {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        if ctx.run.approval_id.is_none() && ctx.index == 0 {
            return Ok(StepOutcome::AwaitApproval {
                action_hash: action_hash(&self.approved)?,
            });
        }
        if ctx.index == 0 {
            let out = self.fire(ctx, "first", &self.first).await?;
            if self.second_key == "next-step" {
                return Ok(StepOutcome::Next {
                    name: "one".into(),
                    output: out,
                });
            }
            if let Err(StepError::Fatal(e)) = self.fire(ctx, self.second_key, &self.second).await {
                *self.second_err.lock().unwrap() = Some(e.code);
            }
            return Ok(StepOutcome::Finish {
                name: "one".into(),
                output: out,
            });
        }
        // Step 1 (only used by the cross-step test): the approval was cleared at checkpoint.
        let out = self.fire(ctx, "second", &self.second).await?;
        Ok(StepOutcome::Finish {
            name: "two".into(),
            output: out,
        })
    }
}

#[tokio::test]
async fn approval_cannot_authorize_different_payload() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let handler = TwoEffects::new(
        json!({"to": "a@b.c"}),
        json!({"to": "evil@b.c"}), // executed payload differs from the approved one
        json!({}),
        "unused",
    );
    let (w, id, ap) = parked_and_granted(
        &db,
        &store,
        handler.clone(),
        &json!({"to": "a@b.c"}),
        in_24h(),
    )
    .await;
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
    assert_eq!(handler.execs.load(SeqCst), 0, "unapproved payload executed");
    assert!(failure_of(&db, id).await.contains("ApprovalPayloadChanged"));
    let consumed: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT consumed_at FROM approvals WHERE id = $1")
            .bind(ap.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        consumed.is_none(),
        "a rejected payload must not burn the approval"
    );
}

#[tokio::test]
async fn approval_cannot_authorize_second_effect() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let payload = json!({"to": "a@b.c"});
    // Same payload (same hash), different effect key, same approval, same step.
    let handler = TwoEffects::new(payload.clone(), payload.clone(), payload.clone(), "again");
    let (w, id, _) = parked_and_granted(&db, &store, handler.clone(), &payload, in_24h()).await;
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
    assert_eq!(handler.execs.load(SeqCst), 1);
    assert_eq!(
        *handler.second_err.lock().unwrap(),
        Some(ErrorCode::Conflict)
    );
}

#[tokio::test]
async fn approval_does_not_carry_into_next_step() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let payload = json!({"to": "a@b.c"});
    let handler = TwoEffects::new(
        payload.clone(),
        payload.clone(),
        payload.clone(),
        "next-step",
    );
    let (w, id, _) = parked_and_granted(&db, &store, handler.clone(), &payload, in_24h()).await;
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
    assert_eq!(handler.execs.load(SeqCst), 1);
    assert!(failure_of(&db, id).await.contains("ApprovalRequired"));
}

// ---------- M5: gated tool calls ----------

struct LateTool {
    fired: AtomicU32,
    reached: AtomicBool,
}

#[async_trait]
impl StepHandler for LateTool {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        self.reached.store(true, SeqCst);
        tokio::time::sleep(Duration::from_millis(900)).await;
        ctx.gated_tool(|| async {
            self.fired.fetch_add(1, SeqCst);
            Ok(())
        })
        .await?;
        Ok(StepOutcome::Finish {
            name: "t".into(),
            output: Value::Null,
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gated_tool_is_lease_guarded_and_counts_persistently() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let handler = Arc::new(LateTool {
        fired: AtomicU32::new(0),
        reached: AtomicBool::new(false),
    });
    let id = store
        .start(input("tool"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("tool", handler.clone());
    let task = tokio::spawn(async move { w.run_once().await });
    wait_until(|| handler.reached.load(SeqCst)).await;
    store.cancel(id).await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(handler.fired.load(SeqCst), 0, "tool ran after cancel");
    assert_eq!(store.get(id).await.unwrap().tool_calls, 0);
}

struct ToolHog;

#[async_trait]
impl StepHandler for ToolHog {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        for _ in 0..21 {
            ctx.gated_tool(|| async { Ok(()) }).await?;
        }
        Ok(StepOutcome::Finish {
            name: "x".into(),
            output: Value::Null,
        })
    }
}

#[tokio::test]
async fn tool_cap_failure_carries_limit_exceeded_code() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let id = store
        .start(input("hog"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("hog", Arc::new(ToolHog));
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
    assert!(failure_of(&db, id).await.starts_with("LimitExceeded"));
    assert_eq!(store.get(id).await.unwrap().tool_calls, 20);
}

// ---------- M4: orphaned intents on terminal runs ----------

struct SlowSender {
    landed: AtomicU32,
    in_exec: AtomicBool,
}

#[async_trait]
impl StepHandler for SlowSender {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let out = ctx
            .effect(
                "send",
                json!({"to": "a@b.c"}),
                || async {
                    self.in_exec.store(true, SeqCst);
                    self.landed.fetch_add(1, SeqCst);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok::<_, EffectError>(json!({"ok": true}))
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

struct Answer(Reconciliation);

#[async_trait]
impl IntentReconciler for Answer {
    async fn reconcile(&self, _intent: Intent) -> pair_core::error::Result<Reconciliation> {
        Ok(match &self.0 {
            Reconciliation::Applied(v) => Reconciliation::Applied(v.clone()),
            Reconciliation::NotApplied => Reconciliation::NotApplied,
        })
    }
}

struct Undecided;

#[async_trait]
impl IntentReconciler for Undecided {
    async fn reconcile(&self, _intent: Intent) -> pair_core::error::Result<Reconciliation> {
        Err(PairError::new(ErrorCode::Internal, "remote unreachable"))
    }
}

async fn intent_status(db: &TestDb) -> String {
    sqlx::query_scalar("SELECT status FROM effect_intents")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn terminal_run_with_unresolved_intent_is_reconciled_by_sweeper() {
    let db = TestDb::new().await;
    // The deadline must outlast the step reaching `exec` even when the machine is loaded; the
    // grace period before the sweeper acts is one lease TTL.
    let store = db.store(JobConfig {
        interactive_timeout: Duration::from_millis(1500),
        lease_ttl: Duration::from_millis(2000),
        ..fast_cfg()
    });
    let handler = Arc::new(SlowSender {
        landed: AtomicU32::new(0),
        in_exec: AtomicBool::new(false),
    });
    let id = store
        .start(input("s"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("s", handler.clone());
    // The deadline cuts the step off mid-effect: the run fails with an explicit marker.
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
    assert!(failure_of(&db, id).await.starts_with("unreconciled_effect"));
    assert_eq!(intent_status(&db).await, "executing");

    // Inside the grace period nothing is swept; an undecided reconciler changes nothing.
    assert_eq!(
        store.reconcile_orphaned_intents(&Undecided).await.unwrap(),
        OrphanSweep::default()
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let undecided = store.reconcile_orphaned_intents(&Undecided).await.unwrap();
    assert_eq!(undecided.undecided, 1);
    assert_eq!(intent_status(&db).await, "executing");

    let applied = store
        .reconcile_orphaned_intents(&Answer(Reconciliation::Applied(json!({"id": "m-9"}))))
        .await
        .unwrap();
    assert_eq!(applied.applied, 1);
    assert_eq!(intent_status(&db).await, "completed");
    let result: Value = sqlx::query_scalar("SELECT result FROM effect_intents")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(result, json!({"id": "m-9"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_run_intent_not_applied_is_settled_by_sweeper() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let handler = Arc::new(SlowSender {
        landed: AtomicU32::new(0),
        in_exec: AtomicBool::new(false),
    });
    let id = store
        .start(input("s"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("s", handler.clone());
    let task = tokio::spawn(async move { w.run_once().await });
    wait_until(|| handler.in_exec.load(SeqCst)).await;
    store.cancel(id).await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(intent_status(&db).await, "executing");
    tokio::time::sleep(Duration::from_millis(900)).await;
    let swept = store
        .reconcile_orphaned_intents(&Answer(Reconciliation::NotApplied))
        .await
        .unwrap();
    assert_eq!(swept.not_applied, 1);
    assert_eq!(intent_status(&db).await, "not_applied");
}

// ---------- LOW: run class is part of the idempotency hash ----------

#[tokio::test]
async fn run_class_is_part_of_the_idempotency_hash() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let key = IdempotencyKey::new();
    let a = store
        .start_with_class(input("t"), key, RunClass::Interactive)
        .await
        .unwrap();
    let again = store
        .start_with_class(input("t"), key, RunClass::Interactive)
        .await
        .unwrap();
    assert_eq!(a, again);
    let err = store
        .start_with_class(input("t"), key, RunClass::Research)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
}
