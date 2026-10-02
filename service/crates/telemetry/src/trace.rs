//! Trace-id propagation: HTTP header codec and a task-local current trace.
use pair_core::ids::TraceId;
use std::future::Future;
use uuid::Uuid;

pub const TRACE_HEADER: &str = "x-pair-trace-id";

tokio::task_local! {
    static CURRENT_TRACE: TraceId;
}

/// Run `fut` with `trace` as the ambient trace id and a tracing span carrying it.
pub async fn with_trace<F: Future>(trace: TraceId, fut: F) -> F::Output {
    use tracing::Instrument;
    let span = tracing::info_span!("trace", trace_id = %trace);
    CURRENT_TRACE.scope(trace, fut.instrument(span)).await
}

pub fn current_trace_id() -> Option<TraceId> {
    CURRENT_TRACE.try_with(|t| *t).ok()
}

pub fn trace_header_value(trace: TraceId) -> String {
    trace.0.to_string()
}

/// Parse an inbound header value; `None` if malformed (caller mints a fresh id).
pub fn parse_trace_header(value: &str) -> Option<TraceId> {
    Uuid::parse_str(value.trim()).ok().map(TraceId)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn with_trace_exposes_current_id_inside_only() {
        let t = TraceId::new();
        assert!(current_trace_id().is_none());
        let seen = with_trace(t, async { current_trace_id() }).await;
        assert_eq!(seen, Some(t));
        assert!(current_trace_id().is_none());
    }

    #[test]
    fn header_roundtrip_and_rejects_garbage() {
        let t = TraceId::new();
        assert_eq!(parse_trace_header(&trace_header_value(t)), Some(t));
        assert_eq!(parse_trace_header("not-a-uuid"), None);
    }
}
