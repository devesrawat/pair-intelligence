#![allow(clippy::unwrap_used)]
//! The ten crash/retry scenarios of spec section 12 (release gate "Recovery"). Catalogue with the
//! injected failure point and expected end state: `evals/datasets/crash_scenarios.md`.
//!
//! The fakes, latches and step handlers live in `tests/common/crash.rs`; this file keeps the ten
//! scenario functions (the catalogue test below reads this file). The two scenarios that need a
//! LIVE worker to notice it lost its lease use a 3 s lease and wait for that event (bounded by the
//! keeper's heartbeat period), not for a duration.
mod common;

use chrono::Utc;
use common::crash::*;
use common::{expire_leases, steady_cfg, TestDb};
use pair_core::error::ErrorCode;
use pair_core::ids::IdempotencyKey;
use pair_core::traits::{Approvals, Workflows};
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{action_hash, PgApprovals, UNRECONCILED_EFFECT};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::sync::Arc;
use tokio::task::JoinSet;

/// 1. Crash before the effect: the worker dies before it even wrote an intent.
#[tokio::test]
async fn crash_scenario_01_crash_before_effect() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let remote = Remote::new();
    let h = Crashy::new(&remote, Some(At::BeforeIntent));
    let id = start(&store).await;

    crash_at(worker(&store, "w1", &h), &h.reached).await;
    assert!(
        intents(&db).await.is_empty(),
        "no intent before the crash point"
    );
    remote.assert_landed(0, "nothing may land before the crash");

    recover(&db, &store, id).await;
    finish_with(&store, &worker(&store, "w2", &h), id).await;
    remote.assert_landed(1, "the retry runs the effect exactly once");
    assert_eq!(
        remote.reconciles.load(SeqCst),
        0,
        "no intent, nothing to reconcile"
    );
    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "completed".into())]
    );
}

/// 2. Crash after the effect landed but before the intent was marked completed.
#[tokio::test]
async fn crash_scenario_02_crash_after_effect_before_completion() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let remote = Remote::new();
    let h = Crashy::new(&remote, Some(At::AfterSend));
    let id = start(&store).await;

    crash_at(worker(&store, "w1", &h), &h.reached).await;
    remote.assert_landed(1, "the effect landed before the crash");
    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "executing".into())]
    );

    recover(&db, &store, id).await;
    finish_with(&store, &worker(&store, "w2", &h), id).await;
    remote.assert_landed(1, "reconcile found it applied; it must not be sent again");
    assert_eq!(remote.execs.load(SeqCst), 1);
    assert_eq!(remote.reconciles.load(SeqCst), 1);
    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "completed".into())]
    );
}

/// 3. Crash between consuming the approval and running the effect.
#[tokio::test]
async fn crash_scenario_03_crash_between_consume_approval_and_effect() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let approvals = PgApprovals::new(db.pool.clone());
    let remote = Remote::new();
    let h = Approved::new(&remote, true);
    let hash = action_hash(&h.payload).unwrap();
    let id = park(&store, &h).await;
    let ap = approvals.approve_default(&hash, "devesh").await.unwrap();
    store.grant(id, ap).await.unwrap();

    crash_at(worker(&store, "w1", &h), &h.reached).await;
    let (consumed, by): (bool, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT consumed_at IS NOT NULL, consumed_by FROM approvals WHERE id = $1")
            .bind(ap.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        consumed && by == Some(id.0),
        "approval spent and bound to the run"
    );
    assert_eq!(
        intents(&db).await,
        vec![("push".into(), "executing".into())]
    );
    remote.assert_landed(0, "crashed before sending");
    assert_eq!(
        approvals.consume(ap, &hash).await.unwrap_err().code,
        ErrorCode::Conflict,
        "a spent approval cannot be reused by anyone else"
    );

    recover(&db, &store, id).await;
    finish_with(&store, &worker(&store, "w2", &h), id).await;
    remote.assert_landed(1, "the same run re-enters its own approval and pushes once");
    assert_eq!(
        remote.reconciles.load(SeqCst),
        1,
        "unresolved intent is reconciled first"
    );
    assert_eq!(
        intents(&db).await,
        vec![("push".into(), "completed".into())]
    );
}

/// 7. The approval expires while the run is parked.
#[tokio::test]
async fn crash_scenario_07_approval_expires_while_parked() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let approvals = PgApprovals::new(db.pool.clone());
    let remote = Remote::new();
    let h = Approved::new(&remote, false);
    let hash = action_hash(&h.payload).unwrap();
    let id = park(&store, &h).await;
    let stale = approvals.approve_default(&hash, "devesh").await.unwrap();
    sqlx::query("UPDATE approvals SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(stale.0)
        .execute(&db.pool)
        .await
        .unwrap();

    assert_eq!(
        store.grant(id, stale).await.unwrap_err().code,
        ErrorCode::ApprovalExpired
    );
    assert_eq!(store.resume(id).await.unwrap(), RunState::WaitingApproval);
    assert_eq!(
        worker(&store, "w", &h).run_once().await.unwrap(),
        None,
        "still parked"
    );
    remote.assert_landed(0, "an expired approval authorizes nothing");
    assert!(intents(&db).await.is_empty());
    assert_eq!(
        approvals.consume(stale, &hash).await.unwrap_err().code,
        ErrorCode::ApprovalExpired
    );

    let fresh = approvals
        .approve(&hash, "devesh", Utc::now() + chrono::Duration::hours(1))
        .await
        .unwrap();
    store.grant(id, fresh).await.unwrap();
    finish_with(&store, &worker(&store, "w", &h), id).await;
    remote.assert_landed(
        1,
        "a fresh approval releases the run; the effect happens once",
    );
}

/// 4. Slow worker: its lease expires while it is still alive and mid-effect; a successor takes over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_scenario_04_slow_worker_after_lease_expiry() {
    let db = TestDb::new().await;
    let store = db.store(watched_cfg());
    let remote = Remote::new();
    let h = Arc::new(Slow {
        remote: remote.clone(),
        calls: AtomicU32::new(0),
        started: Latch::new(),
        cancelled: Latch::new(),
    });
    let id = start(&store).await;

    let w1 = worker(&store, "slow", &h);
    let first = tokio::spawn(async move { w1.run_once().await });
    h.started.wait().await;

    expire_leases(&db).await; // as if it stalled past its TTL; the worker itself is alive
    assert_eq!(store.resume(id).await.unwrap(), RunState::Queued);
    finish_with(&store, &worker(&store, "successor", &h), id).await;

    h.cancelled.wait().await; // the live worker noticed, and its in-flight effect was dropped
    tokio::time::timeout(SYNC_TIMEOUT, first)
        .await
        .expect("slow worker abandons the run")
        .unwrap()
        .unwrap();
    remote.assert_landed(1, "only the successor's effect lands");
    assert_eq!(remote.execs.load(SeqCst), 1);
    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "completed".into())]
    );
}

/// 5. Cancel arrives while the effect is in flight (it already landed, the worker has not noticed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_scenario_05_cancel_during_inflight_effect() {
    let db = TestDb::new().await;
    let store = db.store(watched_cfg());
    let remote = Remote::new();
    let h = Arc::new(LandedThenHeld {
        remote: remote.clone(),
        landed: Latch::new(),
        cancelled: Latch::new(),
    });
    let id = start(&store).await;

    let w1 = worker(&store, "w1", &h);
    let first = tokio::spawn(async move { w1.run_once().await });
    h.landed.wait().await;
    assert_eq!(store.cancel(id).await.unwrap(), RunState::Cancelled);
    h.cancelled.wait().await;
    let (_, state) = tokio::time::timeout(SYNC_TIMEOUT, first)
        .await
        .expect("worker stops after cancel")
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(state, RunState::Cancelled);

    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "executing".into())]
    );
    assert_eq!(
        store.resume(id).await.unwrap(),
        RunState::Cancelled,
        "cancel is terminal"
    );
    assert_eq!(worker(&store, "w2", &h).run_once().await.unwrap(), None);
    remote.assert_landed(1, "the effect that landed is not repeated after cancel");

    assert_eq!(
        sweep_orphans(&db, &store, &remote).await,
        (1, 0, 0),
        "applied, not applied, undecided"
    );
    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "completed".into())]
    );
    remote.assert_landed(1, "settling the orphan never re-executes");
}

/// 6. After a crash mid-effect the reconcile callback itself fails.
#[tokio::test]
async fn crash_scenario_06_reconcile_callback_fails() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let remote = Remote::new();
    let h = Crashy::new(&remote, Some(At::AfterSend));
    let id = start(&store).await;

    crash_at(worker(&store, "w1", &h), &h.reached).await;
    recover(&db, &store, id).await;
    h.reconcile_fails.store(true, SeqCst);
    let w2 = worker(&store, "w2", &h);
    assert_eq!(w2.run_once().await.unwrap(), Some((id, RunState::Failed)));
    let failure: String = sqlx::query_scalar("SELECT failure FROM workflow_runs WHERE id = $1")
        .bind(id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(
        failure.starts_with(UNRECONCILED_EFFECT),
        "flagged, not a clean failure: {failure}"
    );
    assert_eq!(intents(&db).await, vec![("send".into(), "unknown".into())]);
    remote.assert_landed(1, "a failed reconcile must not trigger a blind resend");
    assert_eq!(remote.execs.load(SeqCst), 1);

    backdate_finish(&db).await;
    let sweep = store
        .reconcile_orphaned_intents(remote.as_ref())
        .await
        .unwrap();
    assert_eq!(
        sweep.applied, 1,
        "a later sweep with a working reconciler settles it"
    );
    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "completed".into())]
    );
    remote.assert_landed(1, "still exactly once");
}

/// 8. The same idempotency key is submitted many times, concurrently and after completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_scenario_08_duplicate_start_same_idempotency_key() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let remote = Remote::new();
    let h = Crashy::new(&remote, None);
    let key = IdempotencyKey::new();

    let mut starts = JoinSet::new();
    for _ in 0..8 {
        let s = store.clone();
        starts.spawn(async move { s.start(input(), key).await.unwrap() });
    }
    let mut ids = Vec::new();
    while let Some(id) = starts.join_next().await {
        ids.push(id.unwrap());
    }
    assert!(
        ids.windows(2).all(|p| p[0] == p[1]),
        "one run id for one key: {ids:?}"
    );
    let id = ids[0];
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_runs")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    let different = WorkflowInput {
        kind: KIND.to_owned(),
        payload: json!({"n": 2}),
    };
    assert_eq!(
        store.start(different, key).await.unwrap_err().code,
        ErrorCode::Conflict,
        "same key, different input is refused"
    );

    finish_with(&store, &worker(&store, "w1", &h), id).await;
    assert_eq!(
        store.start(input(), key).await.unwrap(),
        id,
        "late duplicate maps to the finished run"
    );
    assert_eq!(
        worker(&store, "w2", &h).run_once().await.unwrap(),
        None,
        "nothing re-queued"
    );
    remote.assert_landed(1, "eight starts, one effect");
}

/// 9. Four workers race for one queued run while the winner is mid-effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_scenario_09_two_workers_racing_one_run() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let remote = Remote::new();
    let h = Arc::new(Gated {
        remote: remote.clone(),
        entered: Latch::new(),
        release: Latch::new(),
    });
    let id = start(&store).await;

    let mut race = JoinSet::new();
    for n in 0..4 {
        let w = worker(&store, &format!("w{n}"), &h);
        race.spawn(async move { w.run_once().await.unwrap() });
    }
    // The winner is blocked in its effect, so the first three to finish are the losers.
    for _ in 0..3 {
        assert_eq!(
            race.join_next().await.unwrap().unwrap(),
            None,
            "loser must not claim"
        );
    }
    h.entered.wait().await;
    h.release.set();
    assert_eq!(
        race.join_next().await.unwrap().unwrap(),
        Some((id, RunState::Succeeded))
    );
    let epoch: i64 = sqlx::query_scalar("SELECT lease_epoch FROM workflow_runs WHERE id = $1")
        .bind(id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(epoch, 1, "exactly one claim happened");
    remote.assert_landed(1, "one winner, one effect");
}

/// 10. The process dies in the middle of the checkpoint transaction; a restarted process resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_scenario_10_process_restart_mid_checkpoint() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let remote = Remote::new();
    let h = TwoStep::new(&remote);
    let id = start(&store).await;

    kill_process_mid_checkpoint(&db, &h, id).await;

    assert!(
        store.steps(id).await.unwrap().is_empty(),
        "checkpoint did not commit"
    );
    assert_eq!(store.get(id).await.unwrap().next_step, 0);
    remote.assert_landed(1, "the effect landed before the checkpoint");
    assert_eq!(
        intents(&db).await,
        vec![("send".into(), "completed".into())]
    );

    let restarted = restarted_store(&db).await;
    recover(&db, &restarted, id).await;
    finish_with(&restarted, &worker(&restarted, "w2", &h), id).await;
    assert_eq!(
        h.step0_runs.load(SeqCst),
        2,
        "step 0 re-ran after the lost checkpoint"
    );
    remote.assert_landed(1, "its effect was replayed from the intent, not re-sent");
    assert_eq!(remote.execs.load(SeqCst), 1);
    assert_eq!(restarted.steps(id).await.unwrap().len(), 2);
}

/// The catalogue in evals/datasets/crash_scenarios.md and this file must agree: exactly ten rows,
/// each backed by a `crash_scenario_NN_<name>` test.
#[test]
fn crash_scenario_catalogue_matches_tests() {
    const CATALOGUE: &str = include_str!("../../../../evals/datasets/crash_scenarios.md");
    const SOURCE: &str = include_str!("crash_scenarios.rs");
    let rows: Vec<(u32, String)> = CATALOGUE
        .lines()
        .filter_map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            let n = cells.get(1)?.parse::<u32>().ok()?;
            let name = cells.get(2)?.trim_matches('`').to_owned();
            Some((n, name))
        })
        .collect();
    assert_eq!(rows.len(), 10, "catalogue must list exactly ten scenarios");
    for (n, name) in rows {
        let test = format!("async fn crash_scenario_{n:02}_{name}()");
        assert!(SOURCE.contains(&test), "no test for scenario {n}: {test}");
    }
}
