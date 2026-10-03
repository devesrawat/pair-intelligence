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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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

/// The request's in-flight permit, shareable: a handler that detaches work from the request moves a
/// clone into the spawned task, so the slot stays taken until that work ends even if the request
/// is cut off (timeout, disconnect).
#[derive(Clone)]
pub struct InFlightPermit(#[allow(dead_code)] Arc<OwnedSemaphorePermit>);

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
    router.layer(from_fn(move |mut req: Request, next: Next| {
        let permits = Arc::clone(&permits);
        async move {
            let Ok(permit) = permits.try_acquire_owned() else {
                return rejection(StatusCode::SERVICE_UNAVAILABLE, "server is at capacity");
            };
            // This layer keeps its own handle for the life of the request (a handler without
            // extractors drops the request's extensions early); a handler that detaches work
            // takes another and outlives it.
            let held = Arc::new(permit);
            req.extensions_mut()
                .insert(InFlightPermit(Arc::clone(&held)));
            let response = match tokio::time::timeout(timeout, next.run(req)).await {
                Ok(resp) => resp,
                Err(_) => rejection(StatusCode::GATEWAY_TIMEOUT, "request timed out"),
            };
            drop(held);
            response
        }
    }))
}
