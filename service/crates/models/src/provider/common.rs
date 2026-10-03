//! Logic shared by provider adapters: request vetting, deadlines, error mapping, line streaming.
use super::quota::QuotaLimiter;
use super::registry::{Billing, Health, ModelEntry, ProviderKind, ProviderRegistry};
use futures_util::StreamExt;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::money::Price;
use pair_core::types::{ModelMessage, ModelRequest, TrustClass, UsageReport};
use pair_telemetry::Redactor;
use std::future::Future;
use std::time::Duration;

/// Env switch (`1`) that lets calls proceed for entries with `id_verified: false`.
pub const ALLOW_UNVERIFIED_ENV: &str = "PAIR_ALLOW_UNVERIFIED_MODEL_IDS";

pub(crate) fn allow_unverified_from_env() -> bool {
    std::env::var(ALLOW_UNVERIFIED_ENV).is_ok_and(|v| v == "1")
}

/// Max bytes of an upstream error body kept in an error message.
const ERROR_BODY_LIMIT: usize = 512;

/// Resolve and vet the registry entry for a request before any network I/O.
pub(crate) fn vet_request(
    registry: &ProviderRegistry,
    req: &ModelRequest,
    kind: ProviderKind,
    allow_unverified: bool,
) -> Result<ModelEntry> {
    let entry = registry.get(&req.model_id).ok_or_else(|| {
        PairError::new(
            ErrorCode::InvalidInput,
            format!("unknown model id {}", req.model_id),
        )
    })?;
    if entry.provider != kind {
        return Err(PairError::new(
            ErrorCode::InvalidInput,
            format!("model {} is not served by this adapter", entry.id),
        ));
    }
    if entry.health == Health::Disabled {
        return Err(PairError::new(
            ErrorCode::ProviderUnavailable,
            format!("model {} is disabled", entry.id),
        ));
    }
    if !entry.id_verified && !allow_unverified {
        // ProviderDisallowed, not ProviderUnavailable: callers fall through to the next model on
        // "unavailable", so this refusal must stop the call or it would escalate to a pricier tier.
        return Err(PairError::new(
            ErrorCode::ProviderDisallowed,
            format!(
                "model id {} is unverified against the provider catalog; set {ALLOW_UNVERIFIED_ENV}=1 to override",
                entry.id
            ),
        ));
    }
    if !entry.allows(req.data_class) {
        return Err(PairError::new(
            ErrorCode::PolicyDenied,
            format!(
                "data class {:?} not allowed for model {}",
                req.data_class, entry.id
            ),
        ));
    }
    if !entry.paid_execution_enabled() {
        return Err(PairError::new(
            ErrorCode::BudgetUnknownPrice,
            format!(
                "model {} has no configured price; paid execution disabled",
                entry.id
            ),
        ));
    }
    if u64::from(req.max_output_tokens) > entry.max_output_tokens || req.max_output_tokens == 0 {
        return Err(PairError::new(
            ErrorCode::InvalidInput,
            format!(
                "max_output_tokens {} outside 1..={}",
                req.max_output_tokens, entry.max_output_tokens
            ),
        ));
    }
    Ok(entry.clone())
}

/// Actual cost from usage, rounded up. `None` when the price overflows.
/// Role a message may carry on the wire. Only Owner messages keep `system`/`assistant`; Tool and
/// Untrusted content is always downgraded to `user` so it can never read as instructions or as
/// prior assistant output.
pub(crate) fn wire_role(m: &ModelMessage) -> Result<&'static str> {
    let role = match m.role.as_str() {
        "system" => "system",
        "user" => "user",
        "assistant" => "assistant",
        other => {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("unsupported role {other}"),
            ))
        }
    };
    Ok(if m.trust == TrustClass::Owner {
        role
    } else {
        "user"
    })
}

/// Subscription-billed entries are bounded by provider-reported limits and an optional local quota,
/// not by the dollar budget.
pub(crate) fn admit_subscription(entry: &ModelEntry, quota: &QuotaLimiter) -> Result<()> {
    if entry.billing != Billing::Subscription {
        return Ok(());
    }
    // A configured quota is an extra local burst cap; the provider's own reported limits (see
    // `limits.rs`) are what actually bound a subscription.
    match entry.quota_requests_per_minute {
        Some(limit) => quota.admit(&entry.id, limit),
        None => Ok(()),
    }
}

pub(crate) fn usage_report(price: &Price, input_tokens: u64, output_tokens: u64) -> UsageReport {
    UsageReport {
        input_tokens,
        output_tokens,
        actual_cost: price.max_cost(input_tokens, output_tokens),
        price_version: price.version.clone(),
    }
}

/// Apply the request deadline. Dropping the inner future cancels the in-flight HTTP request.
pub(crate) async fn with_deadline<T>(
    deadline_ms: u64,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(Duration::from_millis(deadline_ms), fut).await {
        Ok(r) => r,
        Err(_) => Err(PairError::new(
            ErrorCode::ProviderTimeout,
            format!("provider call exceeded {deadline_ms}ms deadline"),
        )),
    }
}

pub(crate) fn net_error(redactor: &Redactor, e: reqwest::Error) -> PairError {
    let code = if e.is_timeout() {
        ErrorCode::ProviderTimeout
    } else {
        ErrorCode::ProviderUnavailable
    };
    // Strip the URL: it may carry query strings.
    let e = e.without_url();
    PairError::new(
        code,
        redactor.redact(&format!("provider transport error: {e}")),
    )
}

/// Convert a non-2xx response into a redacted, truncated error.
pub(crate) async fn http_error(redactor: &Redactor, resp: reqwest::Response) -> PairError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let mut clipped: String = body.chars().take(ERROR_BODY_LIMIT).collect();
    clipped = redactor.redact(&clipped);
    PairError::new(
        ErrorCode::ProviderUnavailable,
        format!("provider returned HTTP {}: {clipped}", status.as_u16()),
    )
}

/// Incremental splitter for newline-delimited streams (SSE and NDJSON).
#[derive(Debug, Default)]
pub(crate) struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    /// Feed bytes; returns every complete line (without terminator).
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw)
                .trim_end_matches(['\n', '\r'])
                .to_owned();
            lines.push(line);
        }
        lines
    }

    pub(crate) fn finish(self) -> Option<String> {
        if self.buf.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&self.buf).trim().to_owned())
        }
    }
}

/// Drive a streaming body, invoking `on_line` per line until it returns `Ok(true)` (done).
/// Returns whether the stream signalled completion.
pub(crate) async fn read_lines<F>(
    redactor: &Redactor,
    resp: reqwest::Response,
    mut on_line: F,
) -> Result<bool>
where
    F: FnMut(&str) -> Result<bool>,
{
    let mut stream = resp.bytes_stream();
    let mut buf = LineBuffer::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| net_error(redactor, e))?;
        for line in buf.push(&chunk) {
            if on_line(&line)? {
                return Ok(true);
            }
        }
    }
    match buf.finish() {
        Some(rest) => on_line(&rest),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_buffer_splits_across_chunks_and_crlf() {
        let mut b = LineBuffer::default();
        assert!(b.push(b"data: {\"a\"").is_empty());
        let lines = b.push(b":1}\r\n\nrest");
        assert_eq!(lines, vec!["data: {\"a\":1}".to_owned(), String::new()]);
        assert_eq!(b.finish().as_deref(), Some("rest"));
    }
}
