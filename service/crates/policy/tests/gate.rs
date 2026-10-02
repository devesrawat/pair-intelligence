mod common;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{fixture, request};
use pair_core::error::{ErrorCode, Result};
use pair_core::ids::ApprovalId;
use pair_core::traits::{Approvals, Policy};
use pair_core::types::Decision;
use pair_policy::Gate;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct FakeApprovals {
    expected_hash: String,
}

#[async_trait]
impl Approvals for FakeApprovals {
    async fn approve(&self, _h: &str, _a: &str, _e: DateTime<Utc>) -> Result<ApprovalId> {
        Ok(ApprovalId::new())
    }
    async fn consume(&self, _id: ApprovalId, action_hash: &str) -> Result<()> {
        if action_hash == self.expected_hash {
            Ok(())
        } else {
            Err(pair_core::error::PairError::new(
                ErrorCode::ApprovalPayloadChanged,
                "hash mismatch",
            ))
        }
    }
}

#[tokio::test]
async fn gate_does_not_call_closure_on_deny_or_needs_approval() {
    let f = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Gate::new(Arc::new(f.engine.clone()), None);

    for tool in ["unknown.tool", "message.send"] {
        let c = Arc::clone(&calls);
        let res = gate
            .execute(&request(tool), &f.ctx, || async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(res.is_err(), "{tool}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn gate_error_codes_distinguish_deny_from_approval() {
    let f = fixture();
    let gate = Gate::new(Arc::new(f.engine.clone()), None);
    let denied = gate
        .execute(&request("unknown.tool"), &f.ctx, || async { Ok(()) })
        .await;
    assert_eq!(denied.expect_err("deny").code, ErrorCode::PolicyDenied);
    let pending = gate
        .execute(&request("message.send"), &f.ctx, || async { Ok(()) })
        .await;
    assert_eq!(
        pending.expect_err("approval").code,
        ErrorCode::ApprovalRequired
    );
}

#[tokio::test]
async fn gate_runs_closure_on_allow_and_returns_value() {
    let f = fixture();
    let gate = Gate::new(Arc::new(f.engine.clone()), None);
    let out = gate
        .execute(&request("git.commit"), &f.ctx, || async { Ok(42_u32) })
        .await;
    assert_eq!(out.expect("allowed"), 42);
}

#[tokio::test]
async fn gate_runs_closure_only_with_matching_approval() {
    let f = fixture();
    let req = request("message.send");
    let hash = match f.engine.authorize(&req, &f.ctx).decision {
        Decision::NeedsApproval { payload_hash } => payload_hash,
        other => panic!("expected approval, got {other:?}"),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = f.ctx.clone();
    ctx.approvals = vec![ApprovalId::new()];

    let wrong = Gate::new(
        Arc::new(f.engine.clone()),
        Some(Arc::new(FakeApprovals {
            expected_hash: "other".into(),
        })),
    );
    let c = Arc::clone(&calls);
    let res = wrong
        .execute(&req, &ctx, || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(res.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let right = Gate::new(
        Arc::new(f.engine.clone()),
        Some(Arc::new(FakeApprovals {
            expected_hash: hash,
        })),
    );
    let c = Arc::clone(&calls);
    right
        .execute(&req, &ctx, || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("approved");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
