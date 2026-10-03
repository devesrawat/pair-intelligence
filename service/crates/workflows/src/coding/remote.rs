//! Remote writes (push, PR) are never executed by this crate. They resolve to an
//! approval request bound to the exact payload hash; execution belongs to the caller
//! after `Approvals::consume`.
use crate::tools::{GIT_PUSH, PR_CREATE};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{ApprovalId, TaskId, TraceId},
    traits::Policy,
    types::{ActionRequest, DataClass, Decision, PolicyContext},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKind {
    Push,
    OpenPr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAction {
    pub kind: RemoteKind,
    pub remote: String,
    /// Egress host of the remote (e.g. `github.com`); must be on the policy egress list.
    pub host: String,
    pub branch: String,
    pub head_sha: String,
    pub diff_sha256: String,
    /// Declared class of the data being sent (from the repo config).
    pub data_class: DataClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOutcome {
    NeedsApproval {
        payload_hash: String,
    },
    /// Policy allowed it AND an approval id was supplied. The caller must still
    /// consume the approval against `payload_hash` before executing.
    Authorized {
        payload_hash: String,
    },
}

impl RemoteAction {
    pub fn payload_hash(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self)
            .map_err(|e| PairError::new(ErrorCode::Internal, e.to_string()))?;
        Ok(Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }

    fn tool(&self) -> &'static str {
        match self.kind {
            RemoteKind::Push => GIT_PUSH,
            RemoteKind::OpenPr => PR_CREATE,
        }
    }
}

pub fn request_remote_write(
    policy: &dyn Policy,
    workspace_root: &str,
    policy_version: &str,
    task: TaskId,
    trace: TraceId,
    action: &RemoteAction,
    approvals: &[ApprovalId],
) -> Result<RemoteOutcome> {
    let payload_hash = action.payload_hash()?;
    let req = ActionRequest {
        tool: action.tool().to_string(),
        executable: None,
        args: vec![
            format!("payload_hash={payload_hash}"),
            action.branch.clone(),
        ],
        paths: Vec::new(),
        destination: Some(action.host.clone()),
        data_class: action.data_class,
        task,
        trace,
    };
    let ctx = PolicyContext {
        workspace_root: workspace_root.to_string(),
        approvals: approvals.to_vec(),
        policy_version: policy_version.to_string(),
    };
    match policy.authorize(&req, &ctx).decision {
        Decision::Deny { reason } => Err(PairError::new(ErrorCode::PolicyDenied, reason)),
        Decision::NeedsApproval { payload_hash } => {
            Ok(RemoteOutcome::NeedsApproval { payload_hash })
        }
        // Defence in depth: an Allow without any approval id is still not enough.
        Decision::Allow if approvals.is_empty() => {
            Ok(RemoteOutcome::NeedsApproval { payload_hash })
        }
        Decision::Allow => Ok(RemoteOutcome::Authorized { payload_hash }),
    }
}
