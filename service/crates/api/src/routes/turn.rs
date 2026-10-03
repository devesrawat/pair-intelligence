//! `POST /v1/turn`.

use axum::extract::{Extension, State};
use axum::Json;
use pair_core::error::{ErrorCode, PairError};
use serde_json::Value;

use super::{ok, trace_id};
use crate::auth::Actor;
use crate::error::BoundaryError;
use crate::json::ApiJson;
use crate::state::AppState;
use crate::trace::TraceCtx;
use crate::turn::{run_turn, TurnRequest};

pub async fn turn(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(trace): Extension<TraceCtx>,
    ApiJson(body): ApiJson<TurnRequest>,
) -> Result<Json<Value>, BoundaryError> {
    // The data class is judged before anything else, including whether the service is configured.
    let turn = body.validate().map_err(BoundaryError::from_validation)?;
    let services = state.services()?;
    let trace = trace_id(&trace);
    // Detached from the request: the HTTP timeout or a dropped connection must not cancel a turn
    // between reserve and reconcile (that would strand a held reservation and lose the audit row).
    let handle = tokio::spawn(async move { run_turn(&services, &actor.0, trace, turn).await });
    let response = handle.await.map_err(|e| {
        tracing::error!(error = %e, "turn task failed");
        PairError::new(ErrorCode::Internal, "turn task failed")
    })??;
    Ok(ok(response)?)
}
