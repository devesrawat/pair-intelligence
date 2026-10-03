#![allow(dead_code, clippy::unwrap_used)]
//! Fakes, latches, crash helpers and step handlers for the crash scenarios of spec section 12
//! (`crash_scenarios.rs` keeps the scenario test functions, which the catalogue test reads).
//!
//! Method: the external system is a `Remote` that counts effects that actually landed. Crashes are
//! `JoinHandle::abort` (the worker future is dropped, its lease is left behind) or a terminated DB
//! backend. Lease expiry is forced with SQL (`expire_leases`), and tests wait on latches, never on
//! the clock, so nothing here depends on how loaded the machine is.
use super::{expire_leases, steady_cfg, TestDb};
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result as PairResult};
use pair_core::ids::{IdempotencyKey, RunId};
use pair_core::traits::Workflows;
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{
    action_hash, EffectError, Intent, IntentReconciler, JobConfig, JobStore, Reconciliation,
    StepCtx, StepError, StepHandler, StepOutcome, Worker,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// Safety net so a broken scenario fails instead of hanging; never part of the happy path.
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
/// Lease for scenarios where a live worker must detect the loss quickly (heartbeat = ttl / 3).
pub const WATCHED_LEASE: Duration = Duration::from_secs(3);
/// Poll period when waiting on an observable database condition (not on a lease).
pub const DB_POLL: Duration = Duration::from_millis(5);
pub const KIND: &str = "crash";

// ---------- synchronization and fakes ----------

/// One-shot event. `wait` returns once `set` was called (before or after).
pub struct Latch(watch::Sender<bool>);

impl Latch {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(watch::channel(false).0))
    }
    pub fn set(&self) {
        self.0.send_replace(true);
    }
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        tokio::time::timeout(SYNC_TIMEOUT, rx.wait_for(|v| *v))
            .await
            .expect("latch timed out")
            .expect("latch closed");
    }
}

/// Sets its latch when dropped: proves an in-flight future was cancelled.
pub struct OnDrop(Arc<Latch>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.set();
    }
}

/// The outside world: counts effects that landed, and answers reconcile questions truthfully.
#[derive(Default)]
pub struct Remote {
    pub landed: AtomicU32,
    pub execs: AtomicU32,
    pub reconciles: AtomicU32,
}

impl Remote {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn land(&self) -> Value {
        self.execs.fetch_add(1, SeqCst);
        self.landed.fetch_add(1, SeqCst);
        json!({"message_id": "m-1"})
    }
    pub fn truth(&self) -> Reconciliation {
        self.reconciles.fetch_add(1, SeqCst);
        if self.landed.load(SeqCst) > 0 {
            Reconciliation::Applied(json!({"message_id": "m-1"}))
        } else {
            Reconciliation::NotApplied
        }
    }
    pub fn assert_landed(&self, n: u32, why: &str) {
        assert_eq!(self.landed.load(SeqCst), n, "{why}");
    }
}

#[async_trait]
impl IntentReconciler for Remote {
    async fn reconcile(&self, _intent: Intent) -> PairResult<Reconciliation> {
        Ok(self.truth())
    }
}

pub fn input() -> WorkflowInput {
    WorkflowInput {
        kind: KIND.to_owned(),
        payload: json!({"n": 1}),
    }
}

pub fn payload() -> Value {
    json!({"to": "a@b.c"})
}

pub fn worker(store: &JobStore, id: &str, h: &Arc<impl StepHandler + 'static>) -> Worker {
    Worker::new(store.clone(), id).with_handler(KIND, h.clone())
}

pub async fn intents(db: &TestDb) -> Vec<(String, String)> {
    sqlx::query_as("SELECT effect_key, status FROM effect_intents ORDER BY effect_key")
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

/// Run `w` until `reached`, then kill it: the future is dropped, its lease stays behind.
pub async fn crash_at(w: Worker, reached: &Latch) {
    let task = tokio::spawn(async move { w.run_once().await });
    reached.wait().await;
    task.abort();
    let _ = task.await;
}

/// What an operator/supervisor does after a crash: expire, sweep, make runnable.
pub async fn recover(db: &TestDb, store: &JobStore, id: RunId) {
    assert_eq!(store.state(id).await.unwrap(), RunState::Running);
    expire_leases(db).await;
    assert_eq!(store.sweep_expired().await.unwrap(), 1);
    assert_eq!(store.state(id).await.unwrap(), RunState::Interrupted);
    assert_eq!(store.resume(id).await.unwrap(), RunState::Queued);
}

pub async fn finish_with(store: &JobStore, w: &Worker, id: RunId) {
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
    assert_eq!(store.state(id).await.unwrap(), RunState::Succeeded);
}

pub async fn start(store: &JobStore) -> RunId {
    store.start(input(), IdempotencyKey::new()).await.unwrap()
}

// ---------- handler with injectable crash points (scenarios 1, 2, 6) ----------

#[derive(Clone, Copy, PartialEq)]
pub enum At {
    BeforeIntent,
    AfterSend,
}

pub struct Crashy {
    pub remote: Arc<Remote>,
    pub at: Mutex<Option<At>>,
    pub reached: Arc<Latch>,
    pub reconcile_fails: AtomicBool,
}

impl Crashy {
    pub fn new(remote: &Arc<Remote>, at: Option<At>) -> Arc<Self> {
        Arc::new(Self {
            remote: remote.clone(),
            at: Mutex::new(at),
            reached: Latch::new(),
            reconcile_fails: AtomicBool::new(false),
        })
    }
    /// Fires once: the first attempt hangs here, the retry after recovery does not.
    pub fn take(&self, point: At) -> bool {
        let mut at = self.at.lock().unwrap();
        let hit = *at == Some(point);
        if hit {
            *at = None;
        }
        hit
    }
}

#[async_trait]
impl StepHandler for Crashy {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        if self.take(At::BeforeIntent) {
            self.reached.set();
            pending::<()>().await;
        }
        let out = ctx
            .effect(
                "send",
                payload(),
                || async {
                    let result = self.remote.land();
                    if self.take(At::AfterSend) {
                        self.reached.set();
                        pending::<()>().await;
                    }
                    Ok::<_, EffectError>(result)
                },
                |_| async {
                    if self.reconcile_fails.load(SeqCst) {
                        self.remote.reconciles.fetch_add(1, SeqCst);
                        return Err(PairError::new(ErrorCode::Internal, "remote unreachable"));
                    }
                    Ok(self.remote.truth())
                },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

// ---------- handler with an approval-gated effect (scenarios 3 and 7) ----------

pub struct Approved {
    pub remote: Arc<Remote>,
    pub payload: Value,
    pub hang_in_exec: AtomicBool,
    pub reached: Arc<Latch>,
}

impl Approved {
    pub fn new(remote: &Arc<Remote>, hang: bool) -> Arc<Self> {
        Arc::new(Self {
            remote: remote.clone(),
            payload: json!({"branch": "feat/x"}),
            hang_in_exec: AtomicBool::new(hang),
            reached: Latch::new(),
        })
    }
}

#[async_trait]
impl StepHandler for Approved {
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
                    if self.hang_in_exec.swap(false, SeqCst) {
                        self.reached.set();
                        pending::<()>().await;
                    }
                    Ok::<_, EffectError>(self.remote.land())
                },
                |_| async { Ok(self.remote.truth()) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "push".into(),
            output: out,
        })
    }
}

pub async fn park(store: &JobStore, h: &Arc<Approved>) -> RunId {
    let id = start(store).await;
    let w = worker(store, "parker", h);
    assert_eq!(
        w.run_once().await.unwrap(),
        Some((id, RunState::WaitingApproval))
    );
    id
}

// ---------- scenarios with a LIVE worker (4 and 5) ----------

pub fn watched_cfg() -> JobConfig {
    JobConfig {
        lease_ttl: WATCHED_LEASE,
        ..JobConfig::default()
    }
}

/// Effect that blocks forever until its future is dropped. The first call is the slow worker's,
/// later calls (the successor's) land immediately.
pub struct Slow {
    pub remote: Arc<Remote>,
    pub calls: AtomicU32,
    pub started: Arc<Latch>,
    pub cancelled: Arc<Latch>,
}

#[async_trait]
impl StepHandler for Slow {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let out = ctx
            .effect(
                "send",
                payload(),
                || async {
                    if self.calls.fetch_add(1, SeqCst) == 0 {
                        let _cancelled_when_dropped = OnDrop(self.cancelled.clone());
                        self.started.set();
                        pending::<()>().await;
                    }
                    Ok::<_, EffectError>(self.remote.land())
                },
                |_| async { Ok(self.remote.truth()) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

/// Effect that lands remotely, then stays in flight (never returns) until its future is dropped.
pub struct LandedThenHeld {
    pub remote: Arc<Remote>,
    pub landed: Arc<Latch>,
    pub cancelled: Arc<Latch>,
}

#[async_trait]
impl StepHandler for LandedThenHeld {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let out = ctx
            .effect(
                "send",
                payload(),
                || async {
                    let result = self.remote.land();
                    let _cancelled_when_dropped = OnDrop(self.cancelled.clone());
                    self.landed.set();
                    pending::<()>().await;
                    Ok::<_, EffectError>(result)
                },
                |_| async { Ok(self.remote.truth()) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

pub async fn backdate_finish(db: &TestDb) {
    sqlx::query("UPDATE workflow_runs SET finished_at = now() - interval '1 hour'")
        .execute(&db.pool)
        .await
        .unwrap();
}

/// Effect that lands, tells the test it is in, then waits for the release before returning.
pub struct Gated {
    pub remote: Arc<Remote>,
    pub entered: Arc<Latch>,
    pub release: Arc<Latch>,
}

#[async_trait]
impl StepHandler for Gated {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let out = ctx
            .effect(
                "send",
                payload(),
                || async {
                    let result = self.remote.land();
                    self.entered.set();
                    self.release.wait().await;
                    Ok::<_, EffectError>(result)
                },
                |_| async { Ok(self.remote.truth()) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

// ---------- restart mid-checkpoint (scenario 10) ----------

pub struct TwoStep {
    pub remote: Arc<Remote>,
    pub step0_runs: AtomicU32,
    pub block_once: AtomicBool,
    pub effect_done: Arc<Latch>,
    pub proceed: Arc<Latch>,
}

#[async_trait]
impl StepHandler for TwoStep {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        if ctx.index == 1 {
            return Ok(StepOutcome::Finish {
                name: "done".into(),
                output: json!({"ok": true}),
            });
        }
        self.step0_runs.fetch_add(1, SeqCst);
        let out = ctx
            .effect(
                "send",
                payload(),
                || async { Ok::<_, EffectError>(self.remote.land()) },
                |_| async { Ok(self.remote.truth()) },
            )
            .await?;
        if self.block_once.swap(false, SeqCst) {
            self.effect_done.set();
            self.proceed.wait().await;
        }
        Ok(StepOutcome::Next {
            name: "send".into(),
            output: out,
        })
    }
}

/// Wait (by DB state, not by time) until a checkpoint UPDATE is blocked on a row lock; return its pid.
pub async fn blocked_checkpoint_pid(db: &TestDb) -> i32 {
    const PROBE: &str = "SELECT pid FROM pg_stat_activity WHERE datname = current_database() \
        AND wait_event_type = 'Lock' AND query LIKE 'UPDATE workflow_runs SET next_step%' LIMIT 1";
    tokio::time::timeout(SYNC_TIMEOUT, async {
        loop {
            if let Some(pid) = sqlx::query_scalar(PROBE)
                .fetch_optional(&db.pool)
                .await
                .unwrap()
            {
                return pid;
            }
            tokio::time::sleep(DB_POLL).await;
        }
    })
    .await
    .expect("checkpoint never blocked")
}

/// Kill the process in the middle of the step-0 checkpoint transaction of run `id`: hold the run
/// row so the checkpoint blocks, then terminate its database backend.
pub async fn kill_process_mid_checkpoint(db: &TestDb, h: &Arc<TwoStep>, id: RunId) {
    let store = db.store(steady_cfg());
    let w1 = worker(&store, "w1", h);
    let first = tokio::spawn(async move { w1.run_once().await });
    h.effect_done.wait().await;
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM workflow_runs WHERE id = $1 FOR UPDATE")
        .bind(id.0)
        .execute(&mut *holder)
        .await
        .unwrap();
    h.proceed.set();
    let pid = blocked_checkpoint_pid(db).await;
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        first.await.unwrap().is_err(),
        "the crashed process reports a database error"
    );
    holder.rollback().await.unwrap();
}

impl TwoStep {
    pub fn new(remote: &Arc<Remote>) -> Arc<Self> {
        Arc::new(Self {
            remote: remote.clone(),
            step0_runs: AtomicU32::new(0),
            block_once: AtomicBool::new(true),
            effect_done: Latch::new(),
            proceed: Latch::new(),
        })
    }
}

/// A restarted process: new connection pool, new store.
pub async fn restarted_store(db: &TestDb) -> JobStore {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with((*db.pool.connect_options()).clone())
        .await
        .unwrap();
    JobStore::new(pool, steady_cfg())
}

/// Age the finished runs past the sweep grace period and settle their orphaned intents against
/// `remote`; returns `(applied, not_applied, undecided)`.
pub async fn sweep_orphans(db: &TestDb, store: &JobStore, remote: &Arc<Remote>) -> (u64, u64, u64) {
    backdate_finish(db).await;
    let sweep = store
        .reconcile_orphaned_intents(remote.as_ref())
        .await
        .unwrap();
    (sweep.applied, sweep.not_applied, sweep.undecided)
}
