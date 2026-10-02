#![allow(clippy::unwrap_used)]
mod common;

use async_trait::async_trait;
use chrono::Utc;
use common::{fast_cfg, TestDb};
use pair_core::error::ErrorCode;
use pair_core::ids::IdempotencyKey;
use pair_core::traits::{Approvals, Workflows};
use pair_core::types::{RunState, WorkflowInput};
use pair_jobs::{
    action_hash, EffectError, PgApprovals, Reconciliation, StepCtx, StepError, StepHandler,
    StepOutcome, Worker,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;

fn hash_of(v: &Value) -> String {
    action_hash(v).unwrap()
}

#[tokio::test]
async fn expired_approval_is_denied() {
    let db = TestDb::new().await;
    let approvals = PgApprovals::new(db.pool.clone());
    let h = hash_of(&json!({"push": "main"}));
    let id = approvals
        .approve(
            &h,
            "devesh",
            Utc::now() + chrono::Duration::milliseconds(150),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let err = approvals.consume(id, &h).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalExpired);
}

#[tokio::test]
async fn changed_payload_invalidates_approval() {
    let db = TestDb::new().await;
    let approvals = PgApprovals::new(db.pool.clone());
    let approved = hash_of(&json!({"to": "a@b.c", "body": "hi"}));
    let modified = hash_of(&json!({"to": "evil@b.c", "body": "hi"}));
    let id = approvals
        .approve_default(&approved, "devesh")
        .await
        .unwrap();
    let err = approvals.consume(id, &modified).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalPayloadChanged);
    // A rejected attempt does not burn the approval for the exact payload.
    approvals.consume(id, &approved).await.unwrap();
}

#[tokio::test]
async fn approval_single_use() {
    let db = TestDb::new().await;
    let approvals = PgApprovals::new(db.pool.clone());
    let h = hash_of(&json!({"x": 1}));
    let id = approvals.approve_default(&h, "devesh").await.unwrap();
    approvals.consume(id, &h).await.unwrap();
    assert_eq!(
        approvals.consume(id, &h).await.unwrap_err().code,
        ErrorCode::Conflict
    );
}

#[tokio::test]
async fn concurrent_consume_succeeds_exactly_once() {
    let db = TestDb::new().await;
    let approvals = PgApprovals::new(db.pool.clone());
    let h = hash_of(&json!({"x": 2}));
    let id = approvals.approve_default(&h, "devesh").await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (a, h) = (approvals.clone(), h.clone());
        tasks.push(tokio::spawn(async move { a.consume(id, &h).await.is_ok() }));
    }
    let mut ok = 0;
    for t in tasks {
        ok += u32::from(t.await.unwrap());
    }
    assert_eq!(ok, 1);
}

#[tokio::test]
async fn approve_validates_inputs_and_caps_expiry_at_24h() {
    let db = TestDb::new().await;
    let approvals = PgApprovals::new(db.pool.clone());
    let h = hash_of(&json!(1));
    let too_far = Utc::now() + chrono::Duration::hours(25);
    assert_eq!(
        approvals.approve(&h, "d", too_far).await.unwrap_err().code,
        ErrorCode::InvalidInput
    );
    let past = Utc::now() - chrono::Duration::seconds(1);
    assert_eq!(
        approvals.approve(&h, "d", past).await.unwrap_err().code,
        ErrorCode::InvalidInput
    );
    assert_eq!(
        approvals
            .approve_default("nothash", "d")
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
}

// ---------- approval gating a side effect inside a run ----------

struct Pusher {
    pushed: AtomicU32,
    payload: Value,
}

#[async_trait]
impl StepHandler for Pusher {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        let hash = hash_of(&self.payload);
        if ctx.run.approval_id.is_none() {
            return Ok(StepOutcome::AwaitApproval { action_hash: hash });
        }
        ctx.consume_approval(&hash).await?;
        let out = ctx
            .effect(
                "push",
                self.payload.clone(),
                || async {
                    self.pushed.fetch_add(1, SeqCst);
                    Ok::<_, EffectError>(json!({"ok": true}))
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
async fn run_waits_for_approval_then_pushes_once() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let approvals = PgApprovals::new(db.pool.clone());
    let handler = Arc::new(Pusher {
        pushed: AtomicU32::new(0),
        payload: json!({"branch": "feat/x"}),
    });
    let input = WorkflowInput {
        kind: "push".into(),
        payload: json!({}),
    };
    let id = store.start(input, IdempotencyKey::new()).await.unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("push", handler.clone());

    assert_eq!(
        w.run_once().await.unwrap(),
        Some((id, RunState::WaitingApproval))
    );
    assert_eq!(
        store.resume(id).await.unwrap(),
        RunState::WaitingApproval,
        "no approval, stays parked"
    );

    // Approval for a different payload cannot unpark the run.
    let wrong = approvals
        .approve_default(&hash_of(&json!({"branch": "main"})), "devesh")
        .await
        .unwrap();
    assert_eq!(
        store.grant(id, wrong).await.unwrap_err().code,
        ErrorCode::ApprovalPayloadChanged
    );

    let good = approvals
        .approve_default(&hash_of(&handler.payload), "devesh")
        .await
        .unwrap();
    store.grant(id, good).await.unwrap();
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
    assert_eq!(handler.pushed.load(SeqCst), 1);
    // The approval is spent for every other consumer.
    assert!(approvals
        .consume(good, &hash_of(&handler.payload))
        .await
        .is_err());
}

#[tokio::test]
async fn approval_expiring_while_parked_is_rejected_at_grant() {
    let db = TestDb::new().await;
    let store = db.store(fast_cfg());
    let approvals = PgApprovals::new(db.pool.clone());
    let handler = Arc::new(Pusher {
        pushed: AtomicU32::new(0),
        payload: json!({"b": 1}),
    });
    let id = store
        .start(
            WorkflowInput {
                kind: "push".into(),
                payload: json!({}),
            },
            IdempotencyKey::new(),
        )
        .await
        .unwrap();
    let w = Worker::new(store.clone(), "w").with_handler("push", handler.clone());
    w.run_once().await.unwrap();
    let ap = approvals
        .approve(
            &hash_of(&handler.payload),
            "devesh",
            Utc::now() + chrono::Duration::milliseconds(100),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        store.grant(id, ap).await.unwrap_err().code,
        ErrorCode::ApprovalExpired
    );
    assert_eq!(handler.pushed.load(SeqCst), 0);
}
