//! pair-telemetry: trace-id propagation, structured tracing setup, secret redaction, health checks.
pub mod health;
pub mod redact;
pub mod setup;
pub mod trace;

pub use redact::{redact_secrets, Redactor, Secret};
pub use setup::init_tracing;
pub use trace::{
    current_trace_id, parse_trace_header, trace_header_value, with_trace, TRACE_HEADER,
};
