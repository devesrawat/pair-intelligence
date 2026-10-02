use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Gmail,
    Calendar,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::Calendar => "calendar",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "gmail" => Some(Self::Gmail),
            "calendar" => Some(Self::Calendar),
            _ => None,
        }
    }

    /// Kind of source this provider yields.
    pub fn source_kind(self) -> SourceKind {
        match self {
            Self::Gmail => SourceKind::Message,
            Self::Calendar => SourceKind::Event,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Message,
    Event,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Event => "event",
        }
    }
}

/// One remote message or calendar event at a specific revision
/// (Gmail history/etag, Calendar etag/updated).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteItem {
    pub external_id: String,
    pub revision: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SourceChange {
    /// New or changed item.
    Upsert(RemoteItem),
    /// Message deleted or event cancelled/removed remotely.
    Deleted { external_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeBatch {
    pub changes: Vec<SourceChange>,
    pub next_cursor: String,
    pub has_more: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("token revoked or expired by the provider")]
    TokenRevoked,
    #[error("provider unavailable: {0}")]
    Unavailable(String),
}

/// Read-only source of incremental changes. Deliberately has no write methods:
/// external writes are not part of the adapter surface (see `writes`).
#[async_trait]
pub trait SourceClient: Send + Sync {
    /// Changes in `scope` (a Gmail label/folder or a calendar id) since `cursor`.
    /// `None` cursor means an initial sync.
    async fn list_changes(
        &self,
        provider: Provider,
        scope: &str,
        cursor: Option<&str>,
    ) -> Result<ChangeBatch, ClientError>;
}
