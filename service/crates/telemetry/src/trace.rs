//! Trace-id propagation: HTTP header codec and a task-local current trace.
use pair_core::ids::TraceId;
use std::future::Future;
use uuid::Uuid;

/// The one trace header used on every hop (inbound API, outbound provider calls).
pub const TRACE_HEADER: &str = "x-trace-id";
/// Canonical hyphenated UUID text length (so ids always fit a `uuid` column).
const CANONICAL_UUID_LEN: usize = 36;

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

/// Parse an inbound header value; `None` unless it is a canonical hyphenated UUID
/// (caller mints a fresh id). Other `Uuid` spellings (simple, braced, urn) are rejected.
pub fn parse_trace_header(value: &str) -> Option<TraceId> {
    if value.len() != CANONICAL_UUID_LEN {
        return None;
    }
    Uuid::parse_str(value).ok().map(TraceId)
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
    fn trace_header_is_the_single_x_trace_id() {
        assert_eq!(TRACE_HEADER, "x-trace-id");
    }

    #[test]
    fn parse_trace_header_accepts_only_hyphenated_uuid() {
        let id = "0199c0de-1234-7abc-8def-0123456789ab";
        assert!(parse_trace_header(id).is_some());
        assert!(parse_trace_header(&id.to_uppercase()).is_some());
        assert!(parse_trace_header(" 0199c0de-1234-7abc-8def-0123456789ab ").is_none());
        for bad in [
            "0199c0de123470008def0123456789ab",
            "urn:uuid:0199c0de-1234-7abc-8def-0123456789ab",
            "{0199c0de-1234-7abc-8def-0123456789ab}",
            "",
        ] {
            assert!(parse_trace_header(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn header_roundtrip_and_rejects_garbage() {
        let t = TraceId::new();
        assert_eq!(parse_trace_header(&trace_header_value(t)), Some(t));
        assert_eq!(parse_trace_header("not-a-uuid"), None);
    }
}
