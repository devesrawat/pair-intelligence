//! `X-Trace-Id` propagation: reuse a canonical-UUID inbound id, otherwise generate one.
//! Ids are always UUIDs so they can be persisted in `uuid` columns.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use pair_core::ids::TraceId;
pub use pair_telemetry::TRACE_HEADER;
use pair_telemetry::{parse_trace_header, trace_header_value};
use tracing::Instrument;

/// Trace id (canonical UUID text) attached to every request's extensions.
#[derive(Clone, Debug)]
pub struct TraceCtx(pub String);

pub async fn trace_layer(mut req: Request, next: Next) -> Response {
    let trace = req
        .headers()
        .get(TRACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_trace_header)
        .unwrap_or_else(TraceId::new);
    let id = trace_header_value(trace);
    req.extensions_mut().insert(TraceCtx(id.clone()));
    let span = tracing::info_span!(
        "request",
        trace_id = %id,
        method = %req.method(),
        path = %req.uri().path()
    );
    let mut resp = next.run(req).instrument(span).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert(TRACE_HEADER, value);
    }
    resp
}
