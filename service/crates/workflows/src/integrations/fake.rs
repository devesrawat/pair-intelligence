//! Fixture-backed `SourceClient`. Replays scripted pages per scope and records calls.
use super::client::{ChangeBatch, ClientError, Provider, RemoteItem, SourceChange, SourceClient};
use async_trait::async_trait;
use chrono::DateTime;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct FakeClient {
    pages: Mutex<HashMap<String, Vec<Vec<SourceChange>>>>,
    calls: Mutex<Vec<String>>,
    revoked: AtomicBool,
}

impl FakeClient {
    /// Append a page of changes for a scope. The cursor is the index of the next page.
    pub(crate) fn push(&self, scope: &str, changes: Vec<SourceChange>) {
        if let Ok(mut p) = self.pages.lock() {
            p.entry(scope.to_string()).or_default().push(changes);
        }
    }

    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }

    /// Scopes requested so far, in call order.
    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

/// Upsert stamped with a fixed provider time (all such items tie, so the later one applies).
pub(crate) fn upsert(id: &str, revision: &str, title: &str) -> SourceChange {
    upsert_at(id, revision, title, 0)
}

/// Upsert whose provider modification time is `secs` after the epoch.
pub(crate) fn upsert_at(id: &str, revision: &str, title: &str, secs: i64) -> SourceChange {
    SourceChange::Upsert(RemoteItem {
        external_id: id.to_string(),
        revision: revision.to_string(),
        source_updated_at: DateTime::from_timestamp(secs, 0).unwrap_or_default(),
        content: serde_json::json!({ "title": title }),
    })
}

pub(crate) fn deleted(id: &str) -> SourceChange {
    SourceChange::Deleted {
        external_id: id.to_string(),
    }
}

#[async_trait]
impl SourceClient for FakeClient {
    async fn list_changes(
        &self,
        _provider: Provider,
        scope: &str,
        cursor: Option<&str>,
    ) -> Result<ChangeBatch, ClientError> {
        if let Ok(mut c) = self.calls.lock() {
            c.push(scope.to_string());
        }
        if self.revoked.load(Ordering::SeqCst) {
            return Err(ClientError::TokenRevoked);
        }
        let idx: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        let pages = self
            .pages
            .lock()
            .map_err(|_| ClientError::Unavailable("fixture lock poisoned".into()))?;
        let scope_pages = pages.get(scope).map(Vec::as_slice).unwrap_or_default();
        Ok(ChangeBatch {
            changes: scope_pages.get(idx).cloned().unwrap_or_default(),
            next_cursor: (idx + 1).min(scope_pages.len()).to_string(),
            has_more: idx + 1 < scope_pages.len(),
        })
    }
}
