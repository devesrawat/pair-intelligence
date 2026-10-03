//! Background tasks hosted by the binary: drain on shutdown, lease sweeping, readiness liveness.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::StatusCode;
use common::stack::{Stack, StackOpts};
use common::{authed, send, TestDb, TOKEN};
use pair_api::background::tasks::{
    orphan_reconciler, sweeper, worker_loop, ORPHANS_TASK, SWEEPER_TASK, WORKER_TASK,
};
use pair_api::background::{Background, Liveness, TaskSpec, Work};
use pair_api::state::AppState;
use pair_core::ids::IdempotencyKey;
use pair_core::traits::Workflows;
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{JobConfig, JobStore, StepCtx, StepError, StepHandler, StepOutcome};
use serde_json::json;
use tokio::sync::{oneshot, Mutex};

const HUGE_INTERVAL: Duration = Duration::from_secs(3_600);
const KIND: &str = "background-test";

struct FinishAtOnce;

#[async_trait]
impl StepHandler for FinishAtOnce {
    async fn step(&self, _ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        Ok(StepOutcome::Finish {
            name: "done".into(),
            output: json!({"ok": true}),
        })
    }
}

fn spec(name: &'static str, work: Work) -> TaskSpec {
    TaskSpec {
        name,
        interval: HUGE_INTERVAL,
        stall_after: HUGE_INTERVAL,
        work,
    }
}

#[tokio::test]
async fn shutdown_drains_background_tasks() {
    // `slow` is mid-iteration when shutdown arrives: the drain must wait for it to finish.
    let (entered_tx, entered_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let finished = Arc::new(AtomicBool::new(false));
    let slow_finished = finished.clone();
    let slow: Work = Arc::new(move || {
        let (entered, release, finished) = (entered.clone(), release.clone(), slow_finished.clone());
        Box::pin(async move {
            if let Some(tx) = entered.lock().await.take() {
                let _ = tx.send(());
            }
            if let Some(rx) = release.lock().await.take() {
                let _ = rx.await;
            }
            finished.store(true, Ordering::SeqCst);
            Ok(())
        })
    });
    let idle: Work = Arc::new(|| Box::pin(async { Ok(()) }));
    let bg = Arc::new(Background::start(
        vec![spec("slow", slow), spec("idle", idle)],
        Arc::new(Liveness::default()),
    ));

    let ticker = bg.clone();
    let tick = tokio::spawn(async move { ticker.tick_now("slow").await });
    entered_rx.await.expect("slow task started its iteration");
    bg.signal_stop();
    release_tx.send(()).expect("release the slow iteration");
    tick.await.expect("join").expect("the in-flight iteration completed");
    let bg = Arc::try_unwrap(bg).unwrap_or_else(|_| panic!("no other handles remain"));
    let report = bg.drain(Duration::from_secs(10)).await;
    assert!(finished.load(Ordering::SeqCst), "drain must not cut off an in-flight iteration");
    assert_eq!(report.aborted, Vec::<&str>::new());
    assert_eq!(report.drained.len(), 2, "{report:?}");
}

#[tokio::test]
async fn drain_is_bounded_and_aborts_a_stuck_task() {
    let (entered_tx, entered_rx) = oneshot::channel::<()>();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let stuck: Work = Arc::new(move || {
        let entered = entered.clone();
        Box::pin(async move {
            if let Some(tx) = entered.lock().await.take() {
                let _ = tx.send(());
            }
            std::future::pending::<()>().await;
            Ok(())
        })
    });
    // A short interval starts the first iteration on its own; the oneshot is the synchronization.
    let bg = Background::start(
        vec![TaskSpec {
            name: "stuck",
            interval: Duration::from_millis(1),
            stall_after: HUGE_INTERVAL,
            work: stuck,
        }],
        Arc::new(Liveness::default()),
    );
    entered_rx.await.expect("stuck task entered its iteration");
    let report = bg.drain(Duration::from_millis(100)).await;
    assert_eq!(report.aborted, vec!["stuck"]);
    assert!(report.drained.is_empty());
}

#[tokio::test]
async fn sweeper_requeues_expired_lease_in_running_service() {
    let db = TestDb::create_migrated().await;
    let store = JobStore::new(db.pool.clone(), JobConfig::default());
    let handlers: Vec<(String, Arc<dyn StepHandler>)> = vec![(KIND.to_owned(), Arc::new(FinishAtOnce))];
    let mut specs = vec![
        sweeper(store.clone(), HUGE_INTERVAL),
        orphan_reconciler(store.clone(), HUGE_INTERVAL),
    ];
    specs.extend(worker_loop(store.clone(), handlers, HUGE_INTERVAL));
    let liveness = Arc::new(Liveness::default());
    let bg = Background::start(specs, liveness.clone());

    let run = store
        .start(
            WorkflowInput {
                kind: KIND.to_owned(),
                payload: json!({"n": 1}),
            },
            IdempotencyKey::new(),
        )
        .await
        .expect("start");
    // A worker that crashed: it claimed the run, and its lease lapsed.
    let claimed = store.claim("crashed-worker").await.expect("claim").expect("a queued run");
    assert_eq!(claimed.id, run);
    sqlx::query("UPDATE workflow_runs SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(run.0)
        .execute(&db.pool)
        .await
        .expect("expire the lease");
    assert_eq!(store.state(run).await.expect("state"), RunState::Running);

    bg.tick_now(SWEEPER_TASK).await.expect("sweeper tick");
    assert_eq!(store.state(run).await.expect("state"), RunState::Queued, "the sweeper requeues it");

    bg.tick_now(WORKER_TASK).await.expect("worker tick");
    assert_eq!(store.state(run).await.expect("state"), RunState::Succeeded, "and the worker finishes it");

    bg.tick_now(ORPHANS_TASK).await.expect("orphan tick");
    assert!(liveness.statuses().iter().all(|t| !t.stalled), "{:?}", liveness.statuses());
    let report = bg.drain(Duration::from_secs(5)).await;
    assert_eq!(report.drained.len(), 3, "{report:?}");
    db.drop_db().await;
}

#[tokio::test]
async fn worker_loop_without_handlers_never_claims_runs() {
    let db = TestDb::create_migrated().await;
    let store = JobStore::new(db.pool.clone(), JobConfig::default());
    assert!(
        worker_loop(store, Vec::new(), HUGE_INTERVAL).is_none(),
        "a handler-less worker would claim and fail every queued run"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn readyz_reports_background_liveness_and_warns_on_a_stalled_task() {
    let stack = Stack::start(StackOpts::default()).await;
    let liveness = Arc::new(Liveness::default());
    liveness.register(SWEEPER_TASK, HUGE_INTERVAL);
    liveness.tick_ok(SWEEPER_TASK);
    let state = AppState::new(stack.db.pool.clone(), TOKEN)
        .with_liveness(liveness.clone())
        .with_disk_probe(Arc::new(|_| {
            Ok(pair_telemetry::health::DiskStats {
                total_bytes: 100 * pair_telemetry::health::BYTES_PER_GIB,
                free_bytes: 80 * pair_telemetry::health::BYTES_PER_GIB,
            })
        }))
        .with_migrations_dir(common::unique_dir());
    let app = pair_api::router(state);
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    let check = |body: &serde_json::Value| {
        body["checks"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["name"] == "background").cloned())
            .expect("background check present")
    };
    assert_eq!(check(&body)["level"], "ok");

    // A sweeper that never ticks within its window is a warning, not an outage.
    liveness.register_at(SWEEPER_TASK, Duration::ZERO, Instant::now());
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(check(&body)["level"], "warn");
    assert!(check(&body)["detail"].as_str().is_some_and(|d| d.contains(SWEEPER_TASK)));
    stack.finish().await;
}

#[tokio::test]
async fn readyz_is_not_ready_when_services_are_required_but_missing() {
    let db = TestDb::create_migrated().await;
    let state = AppState::new(db.pool.clone(), TOKEN)
        .with_services_required(true)
        .with_migrations_dir(common::unique_dir());
    let app = pair_api::router(state);
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body["checks"]
        .as_array()
        .is_some_and(|c| c.iter().any(|c| c["name"] == "services" && c["level"] == "critical")));
    db.drop_db().await;
}
