//! queries -> source discovery -> capture source versions -> dedupe.
//!
//! The query list and every URL fetched come only from the owner's scope and the search
//! service. Text inside captured pages is never used to choose queries, URLs or tools.
use super::{
    fetch::{normalize_url, FetchOutcome, SourceFetcher},
    types::{ResearchScope, Source},
};
use chrono::Utc;
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{SourceId, TaskId, TraceId},
    traits::Policy,
    types::{ActionRequest, DataClass, Decision, PolicyContext},
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const MAX_QUERIES: usize = 8;
const POLICY_VERSION: &str = "research-workflow";

/// Policy gate for network actions.
pub struct Gate<'a> {
    pub policy: &'a dyn Policy,
    pub task: TaskId,
    pub trace: TraceId,
}

impl Gate<'_> {
    pub fn authorize(&self, tool: &str, destination: &str) -> Result<()> {
        let req = ActionRequest {
            tool: tool.to_string(),
            executable: None,
            args: Vec::new(),
            paths: Vec::new(),
            destination: Some(destination.to_string()),
            data_class: DataClass::Public,
            task: self.task,
            trace: self.trace,
        };
        let ctx = PolicyContext { workspace_root: String::new(), approvals: Vec::new(), policy_version: POLICY_VERSION.into() };
        match self.policy.authorize(&req, &ctx).decision {
            Decision::Allow => Ok(()),
            Decision::Deny { reason } => Err(PairError::new(ErrorCode::PolicyDenied, reason)),
            Decision::NeedsApproval { .. } => Err(PairError::new(ErrorCode::ApprovalRequired, format!("{tool} {destination}"))),
        }
    }
}

pub fn build_queries(scope: &ResearchScope) -> Vec<String> {
    let mut seen = HashSet::new();
    let base = if scope.queries.is_empty() { vec![scope.question.clone()] } else { scope.queries.clone() };
    base.into_iter()
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty() && seen.insert(q.clone()))
        .take(MAX_QUERIES)
        .collect()
}

pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn unavailable(url: &str, normalized: &str, reason: String) -> Source {
    Source {
        id: SourceId::new(),
        url: url.to_string(),
        normalized_url: normalized.to_string(),
        available: false,
        unavailable_reason: Some(reason),
        revision: None,
        content_sha256: None,
        published_at: None,
        fetched_at: Utc::now(),
        text: None,
        duplicate_of: None,
    }
}

async fn capture_one(gate: &Gate<'_>, fetcher: &dyn SourceFetcher, url: &str, normalized: &str) -> Source {
    if let Err(e) = gate.authorize("web_fetch", normalized) {
        return unavailable(url, normalized, format!("blocked by policy: {}", e.message));
    }
    match fetcher.fetch(url).await {
        Err(e) => unavailable(url, normalized, format!("fetch failed: {}", e.message)),
        Ok(FetchOutcome::Unavailable { reason }) => unavailable(url, normalized, reason),
        Ok(FetchOutcome::Page { text, revision, published_at }) => Source {
            id: SourceId::new(),
            url: url.to_string(),
            normalized_url: normalized.to_string(),
            available: true,
            unavailable_reason: None,
            revision,
            content_sha256: Some(sha256_hex(&text)),
            published_at,
            fetched_at: Utc::now(),
            text: Some(text),
            duplicate_of: None,
        },
    }
}

/// Marks later sources with identical content as duplicates of the first.
pub fn dedupe(sources: &mut [Source]) {
    let mut first: HashMap<String, SourceId> = HashMap::new();
    for s in sources.iter_mut().filter(|s| s.available) {
        if let Some(hash) = s.content_sha256.clone() {
            match first.get(&hash) {
                Some(orig) => s.duplicate_of = Some(*orig),
                None => {
                    first.insert(hash, s.id);
                }
            }
        }
    }
}

/// Discovery + capture + dedupe. Returns every attempted source, available or not.
pub async fn capture_sources(gate: &Gate<'_>, fetcher: &dyn SourceFetcher, scope: &ResearchScope) -> Result<Vec<Source>> {
    let mut urls: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for query in build_queries(scope) {
        gate.authorize("web_search", &query)?;
        for cand in fetcher.search(&query).await? {
            match normalize_url(&cand.url) {
                Ok(n) if seen.insert(n.clone()) => urls.push((cand.url, n)),
                Ok(_) => {}
                Err(e) => tracing::warn!(url = %cand.url, error = %e.message, "discarding malformed search result"),
            }
        }
    }
    urls.truncate(scope.max_sources);
    let mut sources = Vec::with_capacity(urls.len());
    for (url, normalized) in &urls {
        sources.push(capture_one(gate, fetcher, url, normalized).await);
    }
    dedupe(&mut sources);
    Ok(sources)
}
