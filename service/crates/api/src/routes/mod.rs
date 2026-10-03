//! The `/v1` endpoints that host the budget, policy, routing and conversation crates.

mod budget;
mod policy;
mod turn;

use axum::routing::post;
use axum::{Json, Router};
use pair_core::error::{ErrorCode, PairError};
use pair_core::ids::TraceId;
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;
use crate::trace::TraceCtx;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/turn", post(turn::turn))
        .route("/v1/budget/reserve", post(budget::reserve))
        .route("/v1/budget/reconcile", post(budget::reconcile))
        .route("/v1/policy/authorize", post(policy::authorize))
        .route("/v1/approvals", post(policy::create_approval))
}

/// Standard success envelope.
pub(crate) fn ok<T: Serialize>(data: T) -> Result<Json<Value>, ApiError> {
    let data = serde_json::to_value(data).map_err(|e| {
        tracing::error!(error = %e, "response serialization failed");
        ApiError(PairError::new(
            ErrorCode::Internal,
            "response could not be built",
        ))
    })?;
    Ok(Json(
        json!({ "success": true, "data": data, "error": null }),
    ))
}

/// The request's trace id as a typed id (the trace layer only ever stores canonical UUIDs).
pub(crate) fn trace_id(trace: &TraceCtx) -> TraceId {
    Uuid::parse_str(&trace.0).map_or_else(|_| TraceId::new(), TraceId)
}
