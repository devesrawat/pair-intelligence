mod common;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{fixture, request, Fixture};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::ApprovalId;
use pair_core::traits::{Approvals, Policy};
use pair_core::types::Decision;
use pair_policy::recorder::{ExecDecision, ExecOutcome, MemoryRecorder};
use pair_policy::Gate;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const SECRET_ARG: &str = "sk-live-4242-DO-NOT-STORE";

struct AcceptAll;

#[async_trait]
impl Approvals for AcceptAll {
    async fn approve(&self, _h: &str, _a: &str, _e: DateTime<Utc>) -> Result<ApprovalId> {
        Ok(ApprovalId::new())
    }
    async fn consume(&self, _id: ApprovalId, _hash: &str) -> Result<()> {
        Ok(())
    }
}

fn gate(f: &Fixture, rec: &Arc<MemoryRecorder>, approvals: bool) -> Gate {
    let approvals: Option<Arc<dyn Approvals>> = approvals.then(|| Arc::new(AcceptAll) as _);
    Gate::new(Arc::new(f.engine.clone()), approvals, rec.clone())
}

fn counter() -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let c = Arc::new(AtomicUsize::new(0));
    (c.clone(), c)
}

#[tokio::test]
async fn every_gate_call_writes_an_execution_row() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = gate(&f, &rec, true);
    let mut with_approval = f.ctx.clone();
    with_approval.approvals = vec![ApprovalId::new()];

    let _ = g
        .execute(&request("unknown.tool"), &f.ctx, || async { Ok(()) })
        .await;
    let _ = Gate::new(Arc::new(f.engine.clone()), None, rec.clone())
        .execute(&request("message.send"), &f.ctx, || async { Ok(()) })
        .await;
    g.execute(&request("git.commit"), &f.ctx, || async { Ok(()) })
        .await
        .expect("allow ok");
    let failed: Result<()> = g
        .execute(&request("git.commit"), &f.ctx, || async {
            Err(PairError::new(ErrorCode::Internal, "tool blew up"))
        })
        .await;
    assert!(failed.is_err());
    g.execute(&request("message.send"), &with_approval, || async {
        Ok(())
    })
    .await
    .expect("approved");

    let rows = rec.rows();
    let got: Vec<_> = rows.iter().map(|r| (r.decision, r.outcome)).collect();
    assert_eq!(
        got,
        vec![
            (ExecDecision::Deny, ExecOutcome::Denied),
            (ExecDecision::NeedsApproval, ExecOutcome::ApprovalRequired),
            (ExecDecision::Allow, ExecOutcome::Ok),
            (ExecDecision::Allow, ExecOutcome::Error),
            (ExecDecision::NeedsApproval, ExecOutcome::Ok),
        ]
    );
    assert_eq!(rows[3].error_code.as_deref(), Some("internal"));
    assert!(rows.iter().all(|r| r.policy_version == f.engine.version()));
}

#[tokio::test]
async fn denied_and_needs_approval_never_run_closure_but_are_recorded() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = Gate::new(Arc::new(f.engine.clone()), None, rec.clone());
    let (calls, shared) = counter();
    for tool in ["unknown.tool", "message.send", "pr.merge"] {
        let c = shared.clone();
        let res = g
            .execute(&request(tool), &f.ctx, || async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(res.is_err(), "{tool}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let rows = rec.rows();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].outcome, ExecOutcome::Denied);
    assert_eq!(rows[0].error_code.as_deref(), Some("policy_denied"));
    assert_eq!(rows[1].outcome, ExecOutcome::ApprovalRequired);
    assert_eq!(rows[1].error_code.as_deref(), Some("approval_required"));
    assert!(rows.iter().all(|r| r.finished));
}

#[tokio::test]
async fn recorder_failure_blocks_execution() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    rec.fail_writes(true);
    let g = gate(&f, &rec, true);
    let (calls, shared) = counter();

    let c = shared.clone();
    let err = g
        .execute(&request("git.commit"), &f.ctx, || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect_err("no audit row, no execution");
    assert_eq!(err.code, ErrorCode::Internal);

    let mut ctx = f.ctx.clone();
    ctx.approvals = vec![ApprovalId::new()];
    let c = shared.clone();
    let err = g
        .execute(&request("message.send"), &ctx, || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect_err("approved path is audited too");
    assert_eq!(err.code, ErrorCode::Internal);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // A refusal stays a refusal when the audit sink is down.
    let denied = g
        .execute(&request("unknown.tool"), &f.ctx, || async { Ok(()) })
        .await
        .expect_err("deny");
    assert_eq!(denied.code, ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn raw_args_never_stored() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = gate(&f, &rec, false);
    let mut req = request("shell.exec");
    req.executable = Some("ls".into());
    req.args = vec!["-l".into(), SECRET_ARG.into()];
    let _ = g.execute(&req, &f.ctx, || async { Ok(()) }).await;

    let rows = rec.rows();
    assert_eq!(rows.len(), 1);
    let dump = format!("{rows:?}");
    assert!(!dump.contains(SECRET_ARG), "raw arg leaked: {dump}");
    let expected = hex::encode(Sha256::digest(
        serde_json::to_vec(&req.args).expect("serialize"),
    ));
    assert_eq!(rows[0].args_hash, expected);
    assert_eq!(rows[0].args_hash.len(), 64);
}

#[tokio::test]
async fn approved_execution_row_carries_trace_task_and_approval_id() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = gate(&f, &rec, true);
    let req = request("message.send");
    let hash = match f.engine.authorize(&req, &f.ctx).decision {
        Decision::NeedsApproval { payload_hash } => payload_hash,
        other => panic!("expected approval, got {other:?}"),
    };
    assert_eq!(hash.len(), 64);
    let mut ctx = f.ctx.clone();
    let approval = ApprovalId::new();
    ctx.approvals = vec![approval];
    g.execute(&req, &ctx, || async { Ok(()) })
        .await
        .expect("approved");
    let rows = rec.rows();
    assert_eq!(rows[0].trace, req.trace);
    assert_eq!(rows[0].task, req.task);
    assert_eq!(rows[0].approval, Some(approval));
}

/// Counts consumptions; each one is single-use in spirit: it is what an outage must not burn.
#[derive(Default)]
struct CountingApprovals {
    consumed: AtomicUsize,
}

#[async_trait]
impl Approvals for CountingApprovals {
    async fn approve(&self, _h: &str, _a: &str, _e: DateTime<Utc>) -> Result<ApprovalId> {
        Ok(ApprovalId::new())
    }
    async fn consume(&self, _id: ApprovalId, _hash: &str) -> Result<()> {
        self.consumed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// An approvals backend that is down (not "this approval is unusable").
struct DownApprovals;

#[async_trait]
impl Approvals for DownApprovals {
    async fn approve(&self, _h: &str, _a: &str, _e: DateTime<Utc>) -> Result<ApprovalId> {
        Err(PairError::new(ErrorCode::Internal, "db down"))
    }
    async fn consume(&self, _id: ApprovalId, _hash: &str) -> Result<()> {
        Err(PairError::new(ErrorCode::Internal, "db down"))
    }
}

#[tokio::test]
async fn recorder_outage_does_not_burn_approval() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    rec.fail_writes(true);
    let approvals = Arc::new(CountingApprovals::default());
    let g = Gate::new(
        Arc::new(f.engine.clone()),
        Some(approvals.clone()),
        rec.clone(),
    );
    let mut ctx = f.ctx.clone();
    ctx.approvals = vec![ApprovalId::new()];
    let (calls, shared) = counter();

    let err = g
        .execute(&request("message.send"), &ctx, || async move {
            shared.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect_err("no audit row, no execution");

    assert_eq!(err.code, ErrorCode::Internal);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        approvals.consumed.load(Ordering::SeqCst),
        0,
        "the single-use approval must survive a recorder outage"
    );
    // Once the recorder is back the very same approval still works.
    rec.fail_writes(false);
    g.execute(&request("message.send"), &ctx, || async { Ok(()) })
        .await
        .expect("approval was not burned");
    assert_eq!(approvals.consumed.load(Ordering::SeqCst), 1);
    assert!(rec.rows().iter().all(|r| r.outcome != ExecOutcome::Started));
}

#[tokio::test]
async fn unusable_approval_closes_the_started_row_as_approval_required() {
    struct RejectAll;
    #[async_trait]
    impl Approvals for RejectAll {
        async fn approve(&self, _h: &str, _a: &str, _e: DateTime<Utc>) -> Result<ApprovalId> {
            Ok(ApprovalId::new())
        }
        async fn consume(&self, _id: ApprovalId, _hash: &str) -> Result<()> {
            Err(PairError::new(ErrorCode::ApprovalExpired, "expired"))
        }
    }
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = Gate::new(
        Arc::new(f.engine.clone()),
        Some(Arc::new(RejectAll)),
        rec.clone(),
    );
    let mut ctx = f.ctx.clone();
    ctx.approvals = vec![ApprovalId::new()];
    let err = g
        .execute(&request("message.send"), &ctx, || async { Ok(()) })
        .await
        .expect_err("expired");
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    let rows = rec.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, ExecOutcome::ApprovalRequired);
    assert!(rows[0].approval.is_none() && rows[0].finished);
}

#[tokio::test]
async fn approvals_backend_outage_is_not_reported_as_approval_required() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = Gate::new(
        Arc::new(f.engine.clone()),
        Some(Arc::new(DownApprovals)),
        rec.clone(),
    );
    let mut ctx = f.ctx.clone();
    ctx.approvals = vec![ApprovalId::new()];
    let (calls, shared) = counter();
    let err = g
        .execute(&request("message.send"), &ctx, || async move {
            shared.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect_err("backend down");
    assert_eq!(
        err.code,
        ErrorCode::Internal,
        "an outage is not a missing approval"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let rows = rec.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, ExecOutcome::Error);
    assert_eq!(rows[0].error_code.as_deref(), Some("approvals_unavailable"));
    assert!(rows[0].finished);
}

#[tokio::test]
async fn gate_requires_a_recorder() {
    // `Gate::new` takes the recorder as its third, mandatory argument; there is no default.
    // (The doc-test on `Gate::new` proves the two-argument form does not compile.)
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = Gate::new(Arc::new(f.engine.clone()), None, rec.clone());
    g.execute(&request("git.commit"), &f.ctx, || async { Ok(()) })
        .await
        .expect("allowed");
    assert_eq!(
        rec.rows().len(),
        1,
        "the gate audits through the recorder it was given"
    );
}

fn request_to(destination: &str) -> pair_core::types::ActionRequest {
    let mut req = request("web.fetch");
    req.destination = Some(destination.to_owned());
    req
}

#[tokio::test]
async fn destination_userinfo_and_query_never_stored() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = gate(&f, &rec, false);
    for dest in [
        "https://user:ghp_TOKEN@api.github.com/repos/x?access_token=SECRET#frag-SECRET",
        "https://ghp_TOKEN@api.github.com/repos/x",
        "api.github.com/repos/x?access_token=SECRET",
        "ssh://git:SECRET@github.com/org/repo.git",
    ] {
        let _ = g
            .execute(&request_to(dest), &f.ctx, || async { Ok(()) })
            .await;
    }
    let _ = g
        .execute(
            &request_to("https://a b.example/?t=SECRET"),
            &f.ctx,
            || async { Ok(()) },
        )
        .await;
    for row in rec.rows() {
        let stored = format!("{row:?}");
        for needle in ["SECRET", "TOKEN", "access_token", "frag"] {
            assert!(!stored.contains(needle), "{needle} leaked: {stored}");
        }
    }
    assert_eq!(
        rec.rows()
            .last()
            .and_then(|r| r.destination.clone())
            .as_deref(),
        Some("[unparseable]")
    );
}

#[tokio::test]
async fn destination_host_and_port_survive_for_audit() {
    let f = fixture();
    let rec = Arc::new(MemoryRecorder::default());
    let g = gate(&f, &rec, false);
    for dest in [
        "https://user:pw@evil.example:8443/a/b?x=1",
        "evil.example:8443",
        "https://api.github.com/repos/x",
    ] {
        let _ = g
            .execute(&request_to(dest), &f.ctx, || async { Ok(()) })
            .await;
    }
    let stored: Vec<Option<String>> = rec.rows().into_iter().map(|r| r.destination).collect();
    assert_eq!(
        stored,
        vec![
            Some("https://evil.example:8443/a/b".to_owned()),
            Some("evil.example:8443".to_owned()),
            Some("https://api.github.com/repos/x".to_owned()),
        ]
    );
}
