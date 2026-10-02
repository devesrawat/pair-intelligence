//! Structured tracing setup.
use tracing_subscriber::{fmt, EnvFilter};

/// Install a global JSON subscriber (idempotent). Level from `RUST_LOG`, default `info`.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Err means a subscriber is already installed; that is fine.
    let _ = fmt().json().with_env_filter(filter).with_current_span(true).try_init();
}
