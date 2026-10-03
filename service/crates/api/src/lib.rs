//! pair-api: HTTP surface. `/healthz` is open; everything else needs the service
//! token and an `X-Actor`. Every response carries `X-Trace-Id`.

pub mod adapter_budget;
pub mod attempts;
pub mod auth;
pub mod background;
pub mod bootstrap;
pub mod config;
pub mod error;
pub mod healthcheck;
pub mod json;
pub mod limits;
pub mod providers;
pub mod readiness;
pub mod routes;
pub mod services;
pub mod shutdown;
pub mod state;
pub mod trace;
pub mod turn;
pub mod wiring;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::StatusCode;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::auth::{require_auth, Actor};
use crate::state::AppState;
use crate::trace::{trace_layer, TraceCtx};

/// No endpoint accepts a request body today; keep the cap small.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Build the full router. Trace layer is outermost so 401s also carry a trace id.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/readyz", get(readyz))
        .route("/v1/whoami", get(whoami))
        .merge(routes::router())
        .route_layer(from_fn_with_state(state.clone(), require_auth))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    // `/healthz` stays outside the limits: the container healthcheck must not be shed under load.
    let protected = limits::apply(protected, state.limits);
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
