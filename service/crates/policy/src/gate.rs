use crate::recorder::{
    error_code_str, ExecDecision, ExecOutcome, ExecutionRecord, ExecutionRecorder, NoopRecorder,
};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, ToolExecutionId};
use pair_core::traits::{Approvals, Policy};
use pair_core::types::{ActionRequest, Decision, PolicyContext};
use std::future::Future;
use std::sync::Arc;

/// `error_code` of a row whose approval could not be checked because the approvals backend failed.
pub const APPROVALS_UNAVAILABLE: &str = "approvals_unavailable";

/// The single execution boundary for tools. A tool body is a closure handed to `execute`;
/// it is only invoked after `Policy::authorize` (and, if required, approval consumption).
///
/// Every call writes an audit row through the [`ExecutionRecorder`] the gate was built with; the
/// recorder is a required constructor argument, so a production gate cannot be unaudited by
/// forgetting a builder call. For anything that would run, the row is written BEFORE the closure
/// and the closure is not invoked if that write fails (fail closed). When an approval is needed
/// the `started` row is written BEFORE the approval is consumed, so a recorder outage cannot burn
/// a single-use approval. If linking the consumed approval to that row then fails, the tool still
/// runs (the approval is already spent for exactly this payload) and the failure is logged; the
/// row stays findable by trace id and `approvals.consumed_at`. Refusals are recorded best-effort
/// and stay refusals either way. Raw arguments are never recorded, only their sha256.
///
/// ```compile_fail
/// # use pair_policy::Gate;
/// # fn build(policy: std::sync::Arc<dyn pair_core::traits::Policy>) {
/// let _ = Gate::new(policy, None); // no recorder: does not compile
/// # }
/// ```
#[derive(Clone)]
pub struct Gate {
    policy: Arc<dyn Policy>,
    approvals: Option<Arc<dyn Approvals>>,
    recorder: Arc<dyn ExecutionRecorder>,
}

/// Why no approval could be consumed: the caller's approvals are unusable, or the backend is down.
struct NoApproval {
    error: PairError,
    outcome: ExecOutcome,
    label: Option<&'static str>,
}

impl Gate {
    /// A gate that audits every call through `recorder`.
    pub fn new(
        policy: Arc<dyn Policy>,
        approvals: Option<Arc<dyn Approvals>>,
        recorder: Arc<dyn ExecutionRecorder>,
    ) -> Self {
        Self {
            policy,
            approvals,
            recorder,
        }
    }

    /// A gate that records nothing. For tests of code that merely runs behind a gate; never for
    /// production wiring (executions would leave no audit trail).
    pub fn unaudited_for_tests(
        policy: Arc<dyn Policy>,
        approvals: Option<Arc<dyn Approvals>>,
    ) -> Self {
        Self::new(policy, approvals, Arc::new(NoopRecorder))
    }

    pub async fn execute<T, F, Fut>(
        &self,
        req: &ActionRequest,
        ctx: &PolicyContext,
        f: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let outcome = self.policy.authorize(req, ctx);
        let version = outcome.policy_version.as_str();
        match outcome.decision {
            Decision::Allow => {
                let decision = ExecDecision::Allow;
                let row = ExecutionRecord::new(req, version, decision, None, ExecOutcome::Started)?;
                self.write_started(req, &row).await?;
                self.run_row(req, row.id, f).await
            }
            Decision::Deny { reason } => {
                let err = PairError::new(ErrorCode::PolicyDenied, reason);
                self.record_refusal(req, version, ExecDecision::Deny, ExecOutcome::Denied, &err)
                    .await;
                Err(err)
            }
            Decision::NeedsApproval { payload_hash } => {
                self.execute_approved(req, ctx, version, &payload_hash, f)
                    .await
            }
        }
    }

    /// Order matters: `started` row, then consume the approval, then link it, then run.
    async fn execute_approved<T, F, Fut>(
        &self,
        req: &ActionRequest,
        ctx: &PolicyContext,
        version: &str,
        payload_hash: &str,
        f: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let decision = ExecDecision::NeedsApproval;
        let Some(approvals) = self
            .approvals
            .as_ref()
            .filter(|_| !ctx.approvals.is_empty())
        else {
            let err = approval_required(payload_hash);
            let outcome = ExecOutcome::ApprovalRequired;
            self.record_refusal(req, version, decision, outcome, &err)
                .await;
            return Err(err);
        };
        let row = ExecutionRecord::new(req, version, decision, None, ExecOutcome::Started)?;
        self.write_started(req, &row).await?;
        match consume_approval(approvals.as_ref(), ctx, payload_hash).await {
            Ok(approval) => {
                if let Err(e) = self.recorder.link_approval(row.id, approval).await {
                    tracing::error!(tool = %req.tool, trace = %req.trace, execution = %row.id, %approval, error = %e, "could not link the consumed approval to its audit row");
                }
                self.run_row(req, row.id, f).await
            }
            Err(no) => {
                let code = no
                    .label
                    .map_or_else(|| error_code_str(no.error.code), str::to_owned);
                if let Err(e) = self.recorder.finish(row.id, no.outcome, Some(&code)).await {
                    tracing::error!(tool = %req.tool, execution = %row.id, error = %e, "audit finish of refused approval failed; row stays 'started'");
                }
                Err(no.error)
            }
        }
    }

    /// No row, no run (and no approval consumed).
    async fn write_started(&self, req: &ActionRequest, row: &ExecutionRecord) -> Result<()> {
        self.recorder.record(row).await.map_err(|e| {
            tracing::error!(tool = %req.tool, trace = %req.trace, error = %e, "audit record failed, execution blocked");
            PairError::new(
                ErrorCode::Internal,
                "audit record could not be written; execution blocked",
            )
        })
    }

    /// Run the tool for an already-written `started` row and close the row.
    async fn run_row<T, F, Fut>(&self, req: &ActionRequest, id: ToolExecutionId, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let result = f().await;
        let (outcome, code) = match &result {
            Ok(_) => (ExecOutcome::Ok, None),
            Err(e) => (ExecOutcome::Error, Some(error_code_str(e.code))),
        };
        if let Err(e) = self.recorder.finish(id, outcome, code.as_deref()).await {
            tracing::error!(tool = %req.tool, trace = %req.trace, execution = %id, error = %e, "audit finish failed; row stays 'started'");
        }
        result
    }

    /// Refusals never execute, so a recorder failure cannot make them less safe; it is logged.
    async fn record_refusal(
        &self,
        req: &ActionRequest,
        version: &str,
        decision: ExecDecision,
        outcome: ExecOutcome,
        err: &PairError,
    ) {
        let row = match ExecutionRecord::new(req, version, decision, None, outcome) {
            Ok(row) => row.with_error(err.code),
            Err(e) => {
                tracing::error!(tool = %req.tool, error = %e, "cannot build audit row for refusal");
                return;
            }
        };
        if let Err(e) = self.recorder.record(&row).await {
            tracing::error!(tool = %req.tool, trace = %req.trace, error = %e, "audit record of refusal failed");
        }
    }
}

fn approval_required(payload_hash: &str) -> PairError {
    PairError::new(
        ErrorCode::ApprovalRequired,
        format!("approval required for payload {payload_hash}"),
    )
}

/// Approvals are single-use and bound to the payload hash recomputed from this exact request.
/// Returns the id of the approval that was consumed. A backend failure (an `Internal` error) is
/// reported as such, never as a missing approval.
async fn consume_approval(
    approvals: &dyn Approvals,
    ctx: &PolicyContext,
    payload_hash: &str,
) -> std::result::Result<ApprovalId, NoApproval> {
    let mut backend_down = false;
    for id in &ctx.approvals {
        match approvals.consume(*id, payload_hash).await {
            Ok(()) => return Ok(*id),
            Err(e) if e.code == ErrorCode::Internal => {
                tracing::error!(approval = %id, error = %e, "approvals backend failed");
                backend_down = true;
            }
            Err(e) => {
                tracing::warn!(approval = %id, error = %e, "approval not usable for this payload")
            }
        }
    }
    Err(if backend_down {
        NoApproval {
            error: PairError::new(ErrorCode::Internal, "approvals backend unavailable"),
            outcome: ExecOutcome::Error,
            label: Some(APPROVALS_UNAVAILABLE),
        }
    } else {
        NoApproval {
            error: approval_required(payload_hash),
            outcome: ExecOutcome::ApprovalRequired,
            label: None,
        }
    })
}
