//! queries -> source discovery -> capture source versions -> dedupe.
//!
//! The query list and every URL fetched come only from the owner's scope and the search
//! service. Text inside captured pages is never used to choose queries, URLs or tools.
use super::{
    fetch::{normalize_url, Candidate, FetchOutcome, SourceFetcher},
    types::{ResearchScope, Source},
};
use chrono::Utc;
use pair_core::{
    error::{ErrorCode, Result},
    ids::{SourceId, TaskId, TraceId},
    types::{ActionRequest, DataClass, PolicyContext},
};
use pair_policy::Gate;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const MAX_QUERIES: usize = 8;
const POLICY_VERSION: &str = "research-workflow";

/// Network access for research. Every search and fetch runs inside `Gate::execute`, so the
/// `SourceFetcher` is only called after policy allows it.
pub struct Network<'a> {
    pub gate: &'a Gate,
    pub fetcher: &'a dyn SourceFetcher,
    pub task: TaskId,
    pub trace: TraceId,
}

impl Network<'_> {
    fn request(&self, tool: &str, destination: &str) -> ActionRequest {
        ActionRequest {
            tool: tool.to_string(),
            executable: None,
            args: Vec::new(),
            paths: Vec::new(),
            destination: Some(destination.to_string()),
            data_class: DataClass::Public,
            task: self.task,
            trace: self.trace,
        }
    }

    fn ctx() -> PolicyContext {
        PolicyContext {
            workspace_root: String::new(),
            approvals: Vec::new(),
            policy_version: POLICY_VERSION.into(),
        }
    }

    async fn search(&self, query: &str) -> Result<Vec<Candidate>> {
        let req = self.request("web_search", query);
        self.gate
            .execute(&req, &Self::ctx(), || self.fetcher.search(query))
            .await
    }

    async fn fetch(&self, url: &str, normalized: &str) -> Result<FetchOutcome> {
        let req = self.request("web_fetch", normalized);
        self.gate
            .execute(&req, &Self::ctx(), || self.fetcher.fetch(url))
            .await
    }
}

pub fn build_queries(scope: &ResearchScope) -> Vec<String> {
    let mut seen = HashSet::new();
    let base = if scope.queries.is_empty() {
        vec![scope.question.clone()]
    } else {
        scope.queries.clone()
    };
    base.into_iter()
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty() && seen.insert(q.clone()))
        .take(MAX_QUERIES)
        .collect()
}

pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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

async fn capture_one(net: &Network<'_>, url: &str, normalized: &str) -> Source {
    match net.fetch(url, normalized).await {
        Err(e) if e.code == ErrorCode::PolicyDenied || e.code == ErrorCode::ApprovalRequired => {
            unavailable(url, normalized, format!("blocked by policy: {}", e.message))
        }
        Err(e) => unavailable(url, normalized, format!("fetch failed: {}", e.message)),
        Ok(FetchOutcome::Unavailable { reason }) => unavailable(url, normalized, reason),
        Ok(FetchOutcome::Page {
            text,
            revision,
            published_at,
        }) => Source {
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
pub async fn capture_sources(net: &Network<'_>, scope: &ResearchScope) -> Result<Vec<Source>> {
    let mut urls: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for query in build_queries(scope) {
        for cand in net.search(&query).await? {
            match normalize_url(&cand.url) {
                Ok(n) if seen.insert(n.clone()) => urls.push((cand.url, n)),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(url = %cand.url, error = %e.message, "discarding malformed search result")
                }
            }
        }
    }
    urls.truncate(scope.max_sources);
    let mut sources = Vec::with_capacity(urls.len());
    for (url, normalized) in &urls {
        sources.push(capture_one(net, url, normalized).await);
    }
    dedupe(&mut sources);
    Ok(sources)
}
