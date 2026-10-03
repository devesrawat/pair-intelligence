//! `POST /v1/policy/authorize` and `POST /v1/approvals`.
//!
//! `authorize` only decides: it never executes anything and never consumes an approval (that
//! happens inside `Gate::execute`, at the execution boundary). Every decision is written to
//! `tool_executions` BEFORE it is returned; if that write fails the call fails (no verdict is
//! handed out that the audit trail does not know about). Approvals can only be created with
//! a second credential, so the party being policed cannot mint its own approvals.

use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::Utc;
use pair_core::error::{ErrorCode, PairError};
use pair_core::ids::{ApprovalId, TaskId};
use pair_core::traits::Policy;
use pair_core::types::{ActionRequest, DataClass, Decision, PolicyContext, PolicyOutcome};
use pair_policy::recorder::{ExecDecision, ExecOutcome, ExecutionRecord, ExecutionRecorder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{ok, trace_id};
use crate::auth::{tokens_match, Actor};
use crate::error::ApiError;
use crate::json::ApiJson;
use crate::state::AppState;
use crate::trace::TraceCtx;

const APPROVER_HEADER: &str = "x-approver-token";
const DEFAULT_APPROVAL_SECS: i64 = 3_600;
const MAX_APPROVAL_SECS: i64 = 24 * 3_600;

/// An `ActionRequest` from the wire. There is deliberately no workspace field: the root comes
/// from server configuration, because a caller-chosen root of `/` would put every path inside it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeBody {
    pub tool: String,
    pub executable: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    pub destination: Option<String>,
    pub data_class: DataClass,
    /// The payload hash covers the task id: reuse one task id for every step of an approval flow.
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub approvals: Vec<Uuid>,
    /// The policy version the caller believes is active; a stale one is denied. Omitted means
    /// "the active version".
    pub policy_version: Option<String>,
}

pub async fn authorize(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceCtx>,
    ApiJson(body): ApiJson<AuthorizeBody>,
) -> Result<Json<Value>, ApiError> {
    let services = state.services()?;
    let request = ActionRequest {
        tool: body.tool,
        executable: body.executable,
        args: body.args,
        paths: body.paths,
        destination: body.destination,
        data_class: body.data_class,
        task: body.task_id.map_or_else(TaskId::new, TaskId),
        trace: trace_id(&trace),
    };
    let context = PolicyContext {
        // An unset or unusable root makes the engine deny (it cannot resolve paths): fail closed.
        workspace_root: services
            .workspace_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        approvals: body.approvals.into_iter().map(ApprovalId).collect(),
        policy_version: body
            .policy_version
            .unwrap_or_else(|| services.policy.version().to_owned()),
    };
    let verdict = services.policy.authorize(&request, &context);
    record_decision(services.recorder.as_ref(), &request, &verdict).await?;
    ok(verdict)
}

/// One terminal audit row per decision. Fails closed: the caller gets an error, not the verdict.
async fn record_decision(
    recorder: &dyn ExecutionRecorder,
    request: &ActionRequest,
    verdict: &PolicyOutcome,
) -> Result<(), ApiError> {
    let (decision, outcome, error) = match &verdict.decision {
        Decision::Allow => (ExecDecision::Allow, ExecOutcome::Authorized, None),
        Decision::Deny { .. } => (
            ExecDecision::Deny,
            ExecOutcome::Denied,
            Some(ErrorCode::PolicyDenied),
        ),
        Decision::NeedsApproval { .. } => (
            ExecDecision::NeedsApproval,
            ExecOutcome::ApprovalRequired,
            Some(ErrorCode::ApprovalRequired),
        ),
    };
    let mut row = ExecutionRecord::new(request, &verdict.policy_version, decision, None, outcome)?;
    if let Some(code) = error {
        row = row.with_error(code);
    }
    recorder.record(&row).await.map_err(|e| {
        tracing::error!(tool = %request.tool, trace = %request.trace, error = %e, "audit record failed; authorize withheld");
        ApiError(PairError::new(
            ErrorCode::Internal,
            "audit record could not be written; authorization withheld",
        ))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalBody {
    pub payload_hash: String,
    pub expires_in_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ApprovalResponse {
    approval_id: String,
    payload_hash: String,
    expires_at: chrono::DateTime<Utc>,
}

fn approver_authorized(headers: &HeaderMap, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    headers
        .get(APPROVER_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|provided| tokens_match(expected, provided.trim()))
}

pub async fn create_approval(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(trace): Extension<TraceCtx>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<ApprovalBody>,
) -> Result<Json<Value>, ApiError> {
    let services = state.services()?;
    if !approver_authorized(&headers, services.approver_token.as_deref()) {
        return Err(ApiError(PairError::new(
            ErrorCode::PolicyDenied,
            "creating an approval needs the approver credential (X-Approver-Token)",
        )));
    }
    let secs = body.expires_in_secs.unwrap_or(DEFAULT_APPROVAL_SECS);
    if !(1..=MAX_APPROVAL_SECS).contains(&secs) {
        return Err(ApiError(PairError::new(
            ErrorCode::InvalidInput,
            "expires_in_secs must be between 1 and 86400 (24 hours)",
        )));
    }
    let expires_at = Utc::now() + chrono::Duration::seconds(secs);
    let id = services
        .approvals
        .approve_with_trace(&body.payload_hash, &actor.0, expires_at, trace_id(&trace))
        .await?;
    ok(ApprovalResponse {
        approval_id: id.to_string(),
        payload_hash: body.payload_hash,
        expires_at,
    })
}
