use crate::recorder::{
    error_code_str, ExecDecision, ExecOutcome, ExecutionRecord, ExecutionRecorder, NoopRecorder,
};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::ApprovalId;
use pair_core::traits::{Approvals, Policy};
use pair_core::types::{ActionRequest, Decision, PolicyContext};
use std::future::Future;
use std::sync::Arc;

/// The single execution boundary for tools. A tool body is a closure handed to `execute`;
/// it is only invoked after `Policy::authorize` (and, if required, approval consumption).
///
/// Every call writes an audit row through the configured [`ExecutionRecorder`]. For anything that
/// would run, the row is written BEFORE the closure and the closure is not invoked if that write
/// fails (fail closed). Refusals are recorded best-effort and stay refusals either way. Raw
/// arguments are never recorded, only their sha256.
#[derive(Clone)]
pub struct Gate {
    policy: Arc<dyn Policy>,
    approvals: Option<Arc<dyn Approvals>>,
    recorder: Arc<dyn ExecutionRecorder>,
}

impl Gate {
    pub fn new(policy: Arc<dyn Policy>, approvals: Option<Arc<dyn Approvals>>) -> Self {
        Self {
            policy,
            approvals,
            recorder: Arc::new(NoopRecorder),
        }
    }

    /// Audit every `execute` call through `recorder`.
    pub fn with_recorder(self, recorder: Arc<dyn ExecutionRecorder>) -> Self {
        Self { recorder, ..self }
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
                self.run_recorded(req, version, ExecDecision::Allow, None, f)
                    .await
            }
            Decision::Deny { reason } => {
                let err = PairError::new(ErrorCode::PolicyDenied, reason);
                self.record_refusal(req, version, ExecDecision::Deny, ExecOutcome::Denied, &err)
                    .await;
                Err(err)
            }
            Decision::NeedsApproval { payload_hash } => {
                match self.consume_approval(ctx, &payload_hash).await {
                    Ok(approval) => {
                        let decision = ExecDecision::NeedsApproval;
                        self.run_recorded(req, version, decision, Some(approval), f)
                            .await
                    }
                    Err(err) => {
                        let (decision, outcome) =
                            (ExecDecision::NeedsApproval, ExecOutcome::ApprovalRequired);
                        self.record_refusal(req, version, decision, outcome, &err)
                            .await;
                        Err(err)
                    }
                }
            }
        }
    }

    /// Write the `started` row, run the tool, close the row. No row, no run.
    async fn run_recorded<T, F, Fut>(
        &self,
        req: &ActionRequest,
        version: &str,
        decision: ExecDecision,
        approval: Option<ApprovalId>,
        f: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let row = ExecutionRecord::new(req, version, decision, approval, ExecOutcome::Started)?;
        if let Err(e) = self.recorder.record(&row).await {
            tracing::error!(tool = %req.tool, trace = %req.trace, error = %e, "audit record failed, execution blocked");
            return Err(PairError::new(
                ErrorCode::Internal,
                "audit record could not be written; execution blocked",
            ));
        }
        let result = f().await;
        let (outcome, code) = match &result {
            Ok(_) => (ExecOutcome::Ok, None),
            Err(e) => (ExecOutcome::Error, Some(error_code_str(e.code))),
        };
        if let Err(e) = self.recorder.finish(row.id, outcome, code.as_deref()).await {
            tracing::error!(tool = %req.tool, trace = %req.trace, execution = %row.id, error = %e, "audit finish failed; row stays 'started'");
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

    /// Approvals are single-use and bound to the payload hash recomputed from this exact request.
    /// Returns the id of the approval that was consumed.
    async fn consume_approval(
        &self,
        ctx: &PolicyContext,
        payload_hash: &str,
    ) -> Result<ApprovalId> {
        let required = || {
            PairError::new(
                ErrorCode::ApprovalRequired,
                format!("approval required for payload {payload_hash}"),
            )
        };
        let approvals = self.approvals.as_ref().ok_or_else(required)?;
        for id in &ctx.approvals {
            match approvals.consume(*id, payload_hash).await {
                Ok(()) => return Ok(*id),
                Err(e) => {
                    tracing::warn!(approval = %id, error = %e, "approval not usable for this payload")
                }
            }
        }
        Err(required())
    }
}
