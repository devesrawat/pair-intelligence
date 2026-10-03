#![allow(clippy::unwrap_used)]
//! `tool_executions` audit rows written by the real gate through `PgExecutionRecorder`, and the
//! trace ids carried by approvals and effect intents (spec sections 2 and 12).
mod common;

use async_trait::async_trait;
use common::{steady_cfg, TestDb};
use pair_core::error::{ErrorCode, PairError};
use pair_core::ids::{IdempotencyKey, TaskId, TraceId};
use pair_core::traits::{Approvals, Policy, Workflows};
use pair_core::types::{
    ActionRequest, DataClass, Decision, PolicyContext, PolicyOutcome, RunState, WorkflowInput,
};
use pair_jobs::{
    action_hash, PgApprovals, PgExecutionRecorder, Reconciliation, StepCtx, StepError, StepHandler,
    StepOutcome, Worker,
};
use pair_policy::Gate;
use serde_json::json;
use sqlx::Row;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::sync::Arc;
use uuid::Uuid;

const SECRET_ARG: &str = "ghp_SUPERSECRETTOKEN_0123456789";
const POLICY_VERSION: &str = "test-policy-1";

/// Decides by tool name: `deny.*` denied, `send.*` needs approval for `hash`, the rest allowed.
struct ByName {
    hash: String,
}

impl Policy for ByName {
    fn authorize(&self, req: &ActionRequest, _ctx: &PolicyContext) -> PolicyOutcome {
        let decision = if req.tool.starts_with("deny.") {
            Decision::Deny {
                reason: "test deny".into(),
            }
        } else if req.tool.starts_with("send.") {
            Decision::NeedsApproval {
                payload_hash: self.hash.clone(),
            }
        } else {
            Decision::Allow
        };
        PolicyOutcome {
            decision,
            policy_version: POLICY_VERSION.to_owned(),
        }
    }
}

fn req(tool: &str, args: &[&str]) -> ActionRequest {
    ActionRequest {
        tool: tool.to_owned(),
        executable: Some("curl".into()),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        paths: vec![],
        destination: Some("api.github.com".into()),
        data_class: DataClass::Public,
        task: TaskId::new(),
        trace: TraceId::new(),
    }
}

fn ctx(approvals: Vec<pair_core::ids::ApprovalId>) -> PolicyContext {
    PolicyContext {
        workspace_root: "/tmp".into(),
        approvals,
        policy_version: POLICY_VERSION.to_owned(),
    }
}

fn gate(db: &TestDb, hash: &str) -> Gate {
    let approvals: Arc<dyn Approvals> = Arc::new(PgApprovals::new(db.pool.clone()));
    Gate::new(Arc::new(ByName { hash: hash.into() }), Some(approvals))
        .with_recorder(Arc::new(PgExecutionRecorder::new(db.pool.clone())))
}

#[tokio::test]
async fn every_gate_call_writes_an_execution_row() {
    let db = TestDb::new().await;
    let hash = action_hash(&json!({"m": 1})).unwrap();
    let g = gate(&db, &hash);
    let approval = PgApprovals::new(db.pool.clone())
        .approve_default(&hash, "devesh")
        .await
        .unwrap();

    let _ = g
        .execute(&req("deny.x", &[]), &ctx(vec![]), || async { Ok(()) })
        .await;
    let _ = g
        .execute(&req("send.x", &[]), &ctx(vec![]), || async { Ok(()) })
        .await;
    g.execute(&req("ok.x", &[]), &ctx(vec![]), || async { Ok(()) })
        .await
        .unwrap();
    let boom: Result<(), _> = g
        .execute(&req("ok.y", &[]), &ctx(vec![]), || async {
            Err(PairError::new(ErrorCode::ProviderTimeout, "boom"))
        })
        .await;
    assert!(boom.is_err());
    g.execute(&req("send.y", &[]), &ctx(vec![approval]), || async {
        Ok(())
    })
    .await
    .unwrap();

    let rows: Vec<(String, String, String, Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT tool, decision, outcome, error_code, approval_id IS NOT NULL, finished_at IS NOT NULL \
         FROM tool_executions ORDER BY started_at, id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let s = |v: &str| v.to_owned();
    assert_eq!(
        rows,
        vec![
            (
                s("deny.x"),
                s("deny"),
                s("denied"),
                Some(s("policy_denied")),
                false,
                true
            ),
            (
                s("send.x"),
                s("needs_approval"),
                s("approval_required"),
                Some(s("approval_required")),
                false,
                true
            ),
            (s("ok.x"), s("allow"), s("ok"), None, false, true),
            (
                s("ok.y"),
                s("allow"),
                s("error"),
                Some(s("provider_timeout")),
                false,
                true
            ),
            (s("send.y"), s("needs_approval"), s("ok"), None, true, true),
        ]
    );
}

#[tokio::test]
async fn raw_args_never_stored() {
    let db = TestDb::new().await;
    let g = gate(&db, &action_hash(&json!(1)).unwrap());
    let r = req("ok.x", &["-H", SECRET_ARG]);
    g.execute(&r, &ctx(vec![]), || async { Ok(()) })
        .await
        .unwrap();

    let dump: String = sqlx::query_scalar("SELECT string_agg(t::text, ' ') FROM tool_executions t")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(
        !dump.contains(SECRET_ARG),
        "raw arg leaked into the row: {dump}"
    );
    let stored: String = sqlx::query_scalar("SELECT args_hash FROM tool_executions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored, pair_policy::recorder::args_hash(&r.args).unwrap());
    assert_eq!(stored.len(), 64);
}

#[tokio::test]
async fn recorder_failure_blocks_execution() {
    let db = TestDb::new().await;
    let broken = sqlx::PgPool::connect_lazy("postgres://pair:pair@127.0.0.1:1/none").unwrap();
    let g = Gate::new(
        Arc::new(ByName {
            hash: String::new(),
        }),
        None,
    )
    .with_recorder(Arc::new(PgExecutionRecorder::new(broken)));
    let ran = Arc::new(AtomicU32::new(0));
    let r = ran.clone();
    let err = g
        .execute(&req("ok.x", &[]), &ctx(vec![]), || async move {
            r.fetch_add(1, SeqCst);
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    assert_eq!(ran.load(SeqCst), 0, "tool ran without an audit row");
    drop(db);
}

async fn insert_row(pool: &sqlx::PgPool, decision: &str, outcome: &str, hash: &str) -> bool {
    sqlx::query(
        "INSERT INTO tool_executions (id, task_id, trace_id, tool, args_hash, data_class, \
         policy_version, decision, outcome, finished_at) \
         VALUES ($1, $2, $3, 't', $4, 'public', 'v', $5, $6, now())",
    )
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(hash)
    .bind(decision)
    .bind(outcome)
    .execute(pool)
    .await
    .is_ok()
}

#[tokio::test]
async fn execution_row_check_constraints_reject_bad_values() {
    let db = TestDb::new().await;
    let good = "a".repeat(64);
    assert!(insert_row(&db.pool, "allow", "ok", &good).await);
    assert!(
        !insert_row(&db.pool, "maybe", "ok", &good).await,
        "decision"
    );
    assert!(
        !insert_row(&db.pool, "allow", "weird", &good).await,
        "outcome"
    );
    assert!(
        !insert_row(&db.pool, "allow", "ok", "not-a-hash").await,
        "hash"
    );
    assert!(
        !insert_row(&db.pool, "allow", "denied", &good).await,
        "denied needs deny"
    );
}

// ---------- trace ids on approvals and effect intents ----------

struct Tracing {
    trace: TraceId,
}

#[async_trait]
impl StepHandler for Tracing {
    async fn step(&self, ctx: &StepCtx) -> Result<StepOutcome, StepError> {
        ctx.bind_trace(self.trace);
        let out = ctx
            .effect(
                "send",
                json!({"to": "a@b.c"}),
                || async { Ok(json!({"ok": true})) },
                |_| async { Ok(Reconciliation::NotApplied) },
            )
            .await?;
        Ok(StepOutcome::Finish {
            name: "send".into(),
            output: out,
        })
    }
}

#[tokio::test]
async fn approval_row_carries_trace_id() {
    let db = TestDb::new().await;
    let trace = TraceId::new();
    let hash = action_hash(&json!({"x": 1})).unwrap();
    let approvals = PgApprovals::new(db.pool.clone());
    let traced = approvals
        .approve_with_trace(
            &hash,
            "devesh",
            chrono::Utc::now() + chrono::Duration::hours(1),
            trace,
        )
        .await
        .unwrap();
    let plain = approvals.approve_default(&hash, "devesh").await.unwrap();

    let stored: Option<Uuid> = sqlx::query_scalar("SELECT trace_id FROM approvals WHERE id = $1")
        .bind(traced.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored, Some(trace.0));
    let untraced: Option<Uuid> = sqlx::query_scalar("SELECT trace_id FROM approvals WHERE id = $1")
        .bind(plain.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(untraced, None, "legacy path stays valid with a null trace");
}

#[tokio::test]
async fn effect_intent_carries_bound_trace_id() {
    let db = TestDb::new().await;
    let store = db.store(steady_cfg());
    let trace = TraceId::new();
    let id = store
        .start(
            WorkflowInput {
                kind: "t".into(),
                payload: json!({}),
            },
            IdempotencyKey::new(),
        )
        .await
        .unwrap();
    let w = Worker::new(store, "w").with_handler("t", Arc::new(Tracing { trace }));
    assert_eq!(w.run_once().await.unwrap(), Some((id, RunState::Succeeded)));
    let row = sqlx::query("SELECT trace_id FROM effect_intents WHERE run_id = $1")
        .bind(id.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let stored: Option<Uuid> = row.try_get("trace_id").unwrap();
    assert_eq!(stored, Some(trace.0));
}
