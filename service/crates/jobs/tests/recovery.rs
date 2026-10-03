#![allow(clippy::unwrap_used)]
mod common;

use async_trait::async_trait;
use common::{expire_leases, steady_cfg, TestDb};
use pair_core::ids::{IdempotencyKey, RunId};
use pair_core::traits::Workflows;
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{
    EffectError, JobConfig, Reconciliation, StepCtx, StepError, StepHandler, StepOutcome, Worker,
};
use serde_json::{json, Value};
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::Arc;
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

// ---------- three-step handler for checkpoint tests ----------

#[derive(Default)]
struct ThreeSteps {
    runs: [AtomicU32; 3],
    block_step1: AtomicBool,
}

#[async_trait]
impl StepHandler for ThreeSteps {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let i = ctx.index as usize;
        self.runs[i].fetch_add(1, SeqCst);
        if i == 1 && self.block_step1.load(SeqCst) {
            pending::<()>().await;
        }
        let name = format!("s{i}");
        let output = json!({ "step": i });
        Ok(if i == 2 {
            StepOutcome::Finish { name, output }
        } else {
            StepOutcome::Next { name, output }
        })
    }
}

#[tokio::test]
async fn restart_resumes_checkpoint() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let handler = Arc::new(ThreeSteps::default());
    handler.block_step1.store(true, SeqCst);
    let id = store
        .start(input("t"), IdempotencyKey::new())
        .await
        .unwrap();

    let w1 = Worker::new(store.clone(), "w1").with_handler("t", handler.clone());
    let task = tokio::spawn(async move { w1.run_once().await });
    wait_until(|| handler.runs[1].load(SeqCst) == 1).await;
    task.abort(); // crash: worker dropped mid-run, lease left behind
    let _ = task.await;

    assert_eq!(store.state(id).await.unwrap(), RunState::Running);
    expire_leases(&db).await;
    assert_eq!(store.sweep_expired().await.unwrap(), 1);
    assert_eq!(store.state(id).await.unwrap(), RunState::Interrupted);
    assert_eq!(store.resume(id).await.unwrap(), RunState::Queued);

    handler.block_step1.store(false, SeqCst);
    let w2 = Worker::new(store.clone(), "w2").with_handler("t", handler.clone());
    assert_eq!(
        w2.run_once().await.unwrap(),
        Some((id, RunState::Succeeded))
    );

    assert_eq!(
        handler.runs[0].load(SeqCst),
        1,
        "completed step must not re-run"
    );
    assert_eq!(handler.runs[1].load(SeqCst), 2);
    assert_eq!(handler.runs[2].load(SeqCst), 1);
    assert_eq!(store.steps(id).await.unwrap().len(), 3);
}

#[tokio::test]
async fn resume_of_expired_running_run_requeues_directly() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let id = store
        .start(input("t"), IdempotencyKey::new())
        .await
        .unwrap();
    assert!(store.claim("ghost").await.unwrap().is_some());
    expire_leases(&db).await;
    assert_eq!(store.resume(id).await.unwrap(), RunState::Queued);
}

// ---------- side-effect handler ----------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    CrashBeforeIntent,
    CrashBeforeExec,
    CrashAfterEffect,
    CrashAfterCompleted,
    AmbiguousApplied,
    AmbiguousNotApplied,
}

struct Sender {
    mode: std::sync::Mutex<Mode>,
    sent: AtomicU32,
    reconciles: AtomicU32,
    reached: AtomicBool,
}

impl Sender {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode: std::sync::Mutex::new(mode),
            sent: AtomicU32::new(0),
            reconciles: AtomicU32::new(0),
            reached: AtomicBool::new(false),
        })
    }
    fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }
    fn set(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
    }
    async fn crash_here(&self) {
        self.reached.store(true, SeqCst);
        pending::<()>().await;
    }
}

#[async_trait]
impl StepHandler for Sender {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        if self.mode() == Mode::CrashBeforeIntent {
            self.crash_here().await;
        }
        let result = ctx
            .effect(
                "send-email",
                json!({"to": "a@b.c"}),
                || async {
                    match self.mode() {
                        Mode::CrashBeforeExec => self.crash_here().await,
                        Mode::AmbiguousNotApplied => {
                            self.set(Mode::Normal);
                            return Err(EffectError::Ambiguous("timeout before send".into()));
                        }
                        _ => {}
                    }
                    self.sent.fetch_add(1, SeqCst);
                    match self.mode() {
                        Mode::CrashAfterEffect => self.crash_here().await,
                        Mode::AmbiguousApplied => {
                            return Err(EffectError::Ambiguous("timeout after send".into()))
                        }
                        _ => {}
                    }
                    Ok(json!({"message_id": "m-1"}))
                },
                |_intent| async {
                    self.reconciles.fetch_add(1, SeqCst);
                    // Stand-in for asking the remote system whether the message exists.
                    Ok(if self.sent.load(SeqCst) > 0 {
                        Reconciliation::Applied(json!({"message_id": "m-1"}))
                    } else {
                        Reconciliation::NotApplied
                    })
                },
            )
            .await?;
        if self.mode() == Mode::CrashAfterCompleted {
            self.crash_here().await;
        }
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: result,
        })
    }
}

/// Run the handler in a worker, crash it once `reached`, recover and finish with a second worker.
async fn crash_and_recover(db: &TestDb, sender: &Arc<Sender>) -> (RunId, RunState) {
    let store = db.store(steady_cfg());
    let id = store
        .start(input("send"), IdempotencyKey::new())
        .await
        .unwrap();
    let w1 = Worker::new(store.clone(), "w1").with_handler("send", sender.clone());
    let task = tokio::spawn(async move { w1.run_once().await });
    wait_until(|| sender.reached.load(SeqCst)).await;
    task.abort();
    let _ = task.await;
    expire_leases(db).await;
    store.sweep_expired().await.unwrap();
    store.resume(id).await.unwrap();
    sender.set(Mode::Normal);
    let w2 = Worker::new(store, "w2").with_handler("send", sender.clone());
    let (rid, state) = w2.run_once().await.unwrap().unwrap();
    assert_eq!(rid, id);
    (id, state)
}

#[tokio::test]
async fn crash_before_side_effect_runs_it_once() {
    let db = TestDb::new().await;
    let sender = Sender::new(Mode::CrashBeforeIntent);
    let (_, state) = crash_and_recover(&db, &sender).await;
    assert_eq!(state, RunState::Succeeded);
    assert_eq!(sender.sent.load(SeqCst), 1);
    assert_eq!(
        sender.reconciles.load(SeqCst),
        0,
        "no intent existed, nothing to reconcile"
    );
}

#[tokio::test]
async fn crash_after_intent_before_effect_reconciles_then_runs_once() {
    let db = TestDb::new().await;
    let sender = Sender::new(Mode::CrashBeforeExec);
    let (_, state) = crash_and_recover(&db, &sender).await;
    assert_eq!(state, RunState::Succeeded);
    assert_eq!(sender.sent.load(SeqCst), 1);
    assert_eq!(sender.reconciles.load(SeqCst), 1);
}

#[tokio::test]
async fn crash_after_side_effect_does_not_repeat_it() {
    let db = TestDb::new().await;
    let sender = Sender::new(Mode::CrashAfterCompleted);
    let (_, state) = crash_and_recover(&db, &sender).await;
    assert_eq!(state, RunState::Succeeded);
    assert_eq!(sender.sent.load(SeqCst), 1);
    assert_eq!(
        sender.reconciles.load(SeqCst),
        0,
        "completed intent is replayed from storage"
    );
}

#[tokio::test]
async fn crash_between_effect_and_completion_is_reconciled_not_repeated() {
    let db = TestDb::new().await;
    let sender = Sender::new(Mode::CrashAfterEffect);
    let (_, state) = crash_and_recover(&db, &sender).await;
    assert_eq!(state, RunState::Succeeded);
    assert_eq!(sender.sent.load(SeqCst), 1);
    assert_eq!(sender.reconciles.load(SeqCst), 1);
}

fn retry_cfg() -> JobConfig {
    let mut cfg = steady_cfg();
    cfg.retry.max_attempts = 2;
    cfg.retry.base = Duration::from_millis(10);
    cfg
}

#[tokio::test]
async fn ambiguous_side_effect_is_reconciled() {
    let db = TestDb::new().await;
    let store = db.store(retry_cfg());
    let sender = Sender::new(Mode::AmbiguousApplied);
    let id = store
        .start(input("send"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("send", sender.clone());
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
    assert_eq!(
        sender.sent.load(SeqCst),
        1,
        "applied effect must not be re-executed"
    );
    assert_eq!(sender.reconciles.load(SeqCst), 1);
}

#[tokio::test]
async fn ambiguous_side_effect_not_applied_is_executed_after_reconcile() {
    let db = TestDb::new().await;
    let store = db.store(retry_cfg());
    let sender = Sender::new(Mode::AmbiguousNotApplied);
    let id = store
        .start(input("send"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("send", sender.clone());
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
    assert_eq!(sender.sent.load(SeqCst), 1);
    assert_eq!(sender.reconciles.load(SeqCst), 1);
}

// ---------- start / claim ----------

#[tokio::test]
async fn start_is_idempotent() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let key = IdempotencyKey::new();
    let (a, b) = tokio::join!(store.start(input("t"), key), store.start(input("t"), key));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a, b);
    assert_eq!(store.start(input("t"), key).await.unwrap(), a);
    let other = WorkflowInput {
        kind: "t".into(),
        payload: json!({"n": 2}),
    };
    assert!(
        store.start(other, key).await.is_err(),
        "same key, different input is a conflict"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_runs")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_ne!(
        store
            .start(input("t"), IdempotencyKey::new())
            .await
            .unwrap(),
        a
    );
}

#[tokio::test]
async fn concurrent_workers_do_not_double_claim() {
    const RUNS: usize = 24;
    const WORKERS: usize = 8;
    let db = TestDb::new().await;
    let store = db.store(JobConfig::default());
    for _ in 0..RUNS {
        store
            .start(input("t"), IdempotencyKey::new())
            .await
            .unwrap();
    }
    let mut tasks = Vec::new();
    for w in 0..WORKERS {
        let s = store.clone();
        tasks.push(tokio::spawn(async move {
            let mut got = Vec::new();
            while let Some(run) = s.claim(&format!("w{w}")).await.unwrap() {
                got.push(run.id);
            }
            got
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    assert_eq!(all.len(), RUNS);
    let unique: std::collections::HashSet<_> = all.iter().map(|r| r.0).collect();
    assert_eq!(unique.len(), RUNS, "a run was claimed twice");
}

// ---------- limits ----------

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
async fn tool_call_cap_fails_the_run_at_20() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let id = store
        .start(input("hog"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("hog", Arc::new(ToolHog));
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
    assert_eq!(store.get(id).await.unwrap().tool_calls, 20);
}

struct Sleeper;
#[async_trait]
impl StepHandler for Sleeper {
    async fn step(&self, _ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(StepOutcome::Finish {
            name: "x".into(),
            output: Value::Null,
        })
    }
}

#[tokio::test]
async fn deadline_fails_the_run() {
    let db = TestDb::new().await;
    let cfg = JobConfig {
        interactive_timeout: Duration::from_millis(150),
        ..steady_cfg()
    };
    let store = db.store(cfg);
    let id = store
        .start(input("sleep"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("sleep", Arc::new(Sleeper));
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
}

#[tokio::test]
async fn research_runs_get_the_longer_timeout() {
    let db = TestDb::new().await;
    let store = db.store(JobConfig::default());
    let research = WorkflowInput {
        kind: "r".into(),
        payload: json!({"run_class": "research"}),
    };
    let a = store.start(research, IdempotencyKey::new()).await.unwrap();
    let b = store
        .start(input("i"), IdempotencyKey::new())
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let (ra, rb) = (store.get(a).await.unwrap(), store.get(b).await.unwrap());
    assert!(ra.deadline_at - now > chrono::Duration::minutes(29));
    assert!(rb.deadline_at - now < chrono::Duration::minutes(16));
}

struct Flaky(AtomicU32);
#[async_trait]
impl StepHandler for Flaky {
    async fn step(&self, _ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        if self.0.fetch_add(1, SeqCst) < 2 {
            return Err(StepError::Transient("blip".into()));
        }
        Ok(StepOutcome::Finish {
            name: "x".into(),
            output: Value::Null,
        })
    }
}

#[tokio::test]
async fn transient_errors_retry_with_capped_backoff_then_succeed() {
    let db = TestDb::new().await;
    let mut cfg = steady_cfg();
    cfg.retry.base = Duration::from_millis(5);
    let store = db.store(cfg);
    let id = store
        .start(input("flaky"), IdempotencyKey::new())
        .await
        .unwrap();
    let w =
        Worker::new(store.clone(), "w").with_handler("flaky", Arc::new(Flaky(AtomicU32::new(0))));
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
}

#[test]
fn backoff_is_capped() {
    let p = JobConfig::default().retry;
    assert_eq!(p.delay(1), p.base);
    assert_eq!(p.delay(2), p.base * 2);
    assert_eq!(p.delay(40), p.cap);
}

// ---------- H4: deadline counts active running time ----------

struct ParkThenFinish {
    active: Duration,
    hash: String,
}

#[async_trait]
impl StepHandler for ParkThenFinish {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        tokio::time::sleep(self.active).await;
        if ctx.run.approval_id.is_none() {
            return Ok(StepOutcome::AwaitApproval {
                action_hash: self.hash.clone(),
            });
        }
        Ok(StepOutcome::Finish {
            name: "x".into(),
            output: Value::Null,
        })
    }
}

async fn park_and_grant(
    db: &TestDb,
    store: &pair_jobs::JobStore,
    handler: Arc<ParkThenFinish>,
    park_for: Duration,
) -> (Worker, RunId) {
    let id = store
        .start(input("park"), IdempotencyKey::new())
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("park", handler.clone());
    assert_eq!(
        w.run_once().await.unwrap(),
        Some((id, RunState::WaitingApproval))
    );
    tokio::time::sleep(park_for).await;
    let ap = pair_jobs::PgApprovals::new(db.pool.clone())
        .approve_default(&handler.hash, "devesh")
        .await
        .unwrap();
    store.grant(id, ap).await.unwrap();
    (w, id)
}

#[tokio::test]
async fn approval_wait_does_not_consume_deadline() {
    let db = TestDb::new().await;
    let store = db.store(JobConfig {
        interactive_timeout: Duration::from_millis(4000),
        ..steady_cfg()
    });
    let handler = Arc::new(ParkThenFinish {
        active: Duration::from_millis(150),
        hash: pair_jobs::action_hash(&json!({"p": 1})).unwrap(),
    });
    // Parked for 4.5s: longer than the whole 4s deadline, but active time is only ~300ms.
    let (w, id) = park_and_grant(&db, &store, handler, Duration::from_millis(4500)).await;
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
}

#[tokio::test]
async fn active_time_before_and_after_approval_still_counts() {
    let db = TestDb::new().await;
    let store = db.store(JobConfig {
        interactive_timeout: Duration::from_millis(4000),
        ..steady_cfg()
    });
    let handler = Arc::new(ParkThenFinish {
        active: Duration::from_millis(2500),
        hash: pair_jobs::action_hash(&json!({"p": 2})).unwrap(),
    });
    // 2.5s active before parking + 2.5s after > 4s of active time (each phase alone is < 4s).
    let (w, id) = park_and_grant(&db, &store, handler, Duration::from_millis(50)).await;
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Failed)));
}
