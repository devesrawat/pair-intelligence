//! Request timeout and in-flight concurrency limit. Excess load is shed with 503 instead of
//! queueing without bound; slow handlers are cut off with 504.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use pair_core::error::ErrorCode;
use serde_json::json;
use tokio::sync::Semaphore;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_IN_FLIGHT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub request_timeout: Duration,
    pub max_in_flight: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        }
    }
}

fn rejection(status: StatusCode, message: &str) -> Response {
    let body = json!({
        "success": false,
        "data": null,
        "error": { "code": ErrorCode::LimitExceeded, "message": message },
    });
    (status, Json(body)).into_response()
}

/// Wrap `router` so each request holds one of `limits.max_in_flight` permits and must finish
/// within `limits.request_timeout`.
pub fn apply<S>(router: Router<S>, limits: Limits) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let permits = Arc::new(Semaphore::new(limits.max_in_flight));
    let timeout = limits.request_timeout;
    router.layer(from_fn(move |req: Request, next: Next| {
        let permits = Arc::clone(&permits);
        async move {
            let Ok(_permit) = permits.try_acquire_owned() else {
                return rejection(StatusCode::SERVICE_UNAVAILABLE, "server is at capacity");
            };
            match tokio::time::timeout(timeout, next.run(req)).await {
                Ok(resp) => resp,
                Err(_) => rejection(StatusCode::GATEWAY_TIMEOUT, "request timed out"),
            }
        }
    }))
}
