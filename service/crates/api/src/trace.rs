//! `X-Trace-Id` propagation: reuse a well-formed inbound id, otherwise generate one.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use pair_core::ids::TraceId;
use tracing::Instrument;

pub const TRACE_HEADER: &str = "x-trace-id";
const MAX_TRACE_ID_LEN: usize = 64;

/// Trace id attached to every request's extensions.
#[derive(Clone, Debug)]
pub struct TraceCtx(pub String);

fn is_valid_trace_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_TRACE_ID_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub async fn trace_layer(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(TRACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_valid_trace_id(s))
        .map(str::to_owned)
        .unwrap_or_else(|| TraceId::new().to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_trace_id_rejects_unsafe_values() {
        assert!(is_valid_trace_id("abc-123_X"));
        assert!(!is_valid_trace_id(""));
        assert!(!is_valid_trace_id("a b"));
        assert!(!is_valid_trace_id("a\nb"));
        assert!(!is_valid_trace_id(&"a".repeat(MAX_TRACE_ID_LEN + 1)));
    }
}
