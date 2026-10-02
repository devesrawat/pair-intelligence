//! pair-api: HTTP surface. `/healthz` is open; everything else needs the service
//! token and an `X-Actor`. Every response carries `X-Trace-Id`.

pub mod auth;
pub mod config;
pub mod error;
pub mod providers;
pub mod readiness;
pub mod state;
pub mod trace;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::auth::{require_auth, Actor};
use crate::state::AppState;
use crate::trace::{trace_layer, TraceCtx};

/// Build the full router. Trace layer is outermost so 401s also carry a trace id.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/readyz", get(readyz))
        .route("/v1/whoami", get(whoami))
        .route_layer(from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/healthz", get(healthz))
        .merge(protected)
        .with_state(state)
        .layer(from_fn(trace_layer))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn readyz(State(state): State<AppState>) -> Response {
    let report = readiness::build_report(&state).await;
    let status = if report.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(report)).into_response()
}

async fn whoami(
    Extension(actor): Extension<Actor>,
    Extension(trace): Extension<TraceCtx>,
) -> Json<serde_json::Value> {
    Json(json!({
        "success": true,
        "data": { "actor": actor.0, "trace_id": trace.0 },
        "error": null,
    }))
}
