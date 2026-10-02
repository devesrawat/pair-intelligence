use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::traits::{Approvals, Policy};
use pair_core::types::{ActionRequest, Decision, PolicyContext};
use std::future::Future;
use std::sync::Arc;

/// The single execution boundary for tools. A tool body is a closure handed to `execute`;
/// it is only invoked after `Policy::authorize` (and, if required, approval consumption).
#[derive(Clone)]
pub struct Gate {
    policy: Arc<dyn Policy>,
    approvals: Option<Arc<dyn Approvals>>,
}

impl Gate {
    pub fn new(policy: Arc<dyn Policy>, approvals: Option<Arc<dyn Approvals>>) -> Self {
        Self { policy, approvals }
    }

    pub async fn execute<T, F, Fut>(&self, req: &ActionRequest, ctx: &PolicyContext, f: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        match self.policy.authorize(req, ctx).decision {
            Decision::Allow => f().await,
            Decision::Deny { reason } => Err(PairError::new(ErrorCode::PolicyDenied, reason)),
            Decision::NeedsApproval { payload_hash } => {
                self.consume_approval(ctx, &payload_hash).await?;
                f().await
            }
        }
    }

    /// Approvals are single-use and bound to the payload hash recomputed from this exact request.
    async fn consume_approval(&self, ctx: &PolicyContext, payload_hash: &str) -> Result<()> {
        let required = || PairError::new(ErrorCode::ApprovalRequired, format!("approval required for payload {payload_hash}"));
        let approvals = self.approvals.as_ref().ok_or_else(required)?;
        for id in &ctx.approvals {
            match approvals.consume(*id, payload_hash).await {
                Ok(()) => return Ok(()),
                Err(e) => tracing::warn!(approval = %id, error = %e, "approval not usable for this payload"),
            }
        }
        Err(required())
    }
}
