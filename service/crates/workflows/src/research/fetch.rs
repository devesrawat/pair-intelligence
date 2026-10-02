//! Source discovery / capture contract (external service, injected) and URL handling.
use async_trait::async_trait;
use chrono::NaiveDate;
use pair_core::error::{ErrorCode, PairError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum FetchOutcome {
    Page { text: String, revision: Option<String>, published_at: Option<NaiveDate> },
    Unavailable { reason: String },
}

#[async_trait]
pub trait SourceFetcher: Send + Sync {
    async fn search(&self, query: &str) -> Result<Vec<Candidate>>;
    async fn fetch(&self, url: &str) -> Result<FetchOutcome>;
}

/// Canonical form used for dedupe and for the invented-URL check.
pub fn normalize_url(url: &str) -> Result<String> {
    let bad = |m: &str| PairError::new(ErrorCode::InvalidInput, format!("{m}: {url:?}"));
    let trimmed = url.trim();
    let (scheme, rest) = trimmed.split_once("://").ok_or_else(|| bad("not an absolute URL"))?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => ":80",
        "https" => ":443",
        _ => return Err(bad("unsupported scheme")),
    };
    let rest = rest.split('#').next().unwrap_or_default();
    let split_at = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(split_at);
    if authority.is_empty() || authority.contains('@') || authority.contains(char::is_whitespace) {
        return Err(bad("invalid authority"));
    }
    let authority = authority.to_ascii_lowercase();
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority);
    let tail = if tail.is_empty() { "/" } else { tail };
    let tail = if tail.len() > 1 && !tail.contains('?') { tail.trim_end_matches('/') } else { tail };
    let tail = if tail.is_empty() { "/" } else { tail };
    Ok(format!("{scheme}://{authority}{tail}"))
}
