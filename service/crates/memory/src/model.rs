//! Plain data types of the memory store.
use chrono::{DateTime, Utc};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{MemoryId, SourceId},
    types::{DataClass, MemoryCandidate, TrustClass},
};
use serde::{Deserialize, Serialize};

/// Memory types from spec section 7.
pub const MEMORY_KINDS: [&str; 10] = [
    "fact",
    "preference",
    "project",
    "person",
    "decision",
    "procedure",
    "goal",
    "event",
    "commitment",
    "open_loop",
];

pub fn validate_kind(kind: &str) -> Result<()> {
    if MEMORY_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(crate::error::invalid(format!(
            "unknown memory kind '{kind}'"
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Accepted,
    Superseded,
    Expired,
}

impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "accepted" => Ok(Self::Accepted),
            "superseded" => Ok(Self::Superseded),
            "expired" => Ok(Self::Expired),
            other => Err(PairError::new(
                ErrorCode::Internal,
                format!("unknown memory status '{other}' in database"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Visible,
    Hidden,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
        }
    }
}

pub fn data_class_str(c: DataClass) -> &'static str {
    match c {
        DataClass::Public => "public",
        DataClass::Personal => "personal",
        DataClass::Sensitive => "sensitive",
        DataClass::Employer => "employer",
    }
}

pub fn parse_data_class(s: &str) -> Result<DataClass> {
    match s {
        "public" => Ok(DataClass::Public),
        "personal" => Ok(DataClass::Personal),
        "sensitive" => Ok(DataClass::Sensitive),
        "employer" => Ok(DataClass::Employer),
        other => Err(crate::error::invalid(format!(
            "unknown data class '{other}'"
        ))),
    }
}

pub fn trust_str(t: TrustClass) -> &'static str {
    match t {
        TrustClass::Owner => "owner",
        TrustClass::Tool => "tool",
        TrustClass::Untrusted => "untrusted",
    }
}

pub fn parse_trust(s: &str) -> Result<TrustClass> {
    match s {
        "owner" => Ok(TrustClass::Owner),
        "tool" => Ok(TrustClass::Tool),
        "untrusted" => Ok(TrustClass::Untrusted),
        other => Err(crate::error::invalid(format!(
            "unknown trust class '{other}'"
        ))),
    }
}

#[derive(Debug, Clone)]
pub struct NewSource {
    pub kind: String,
    pub external_id: String,
    pub content_hash: String,
    pub data_class: DataClass,
    pub trust: TrustClass,
    pub uri: Option<String>,
    pub captured_at: Option<DateTime<Utc>>,
}

impl NewSource {
    pub fn new(
        kind: impl Into<String>,
        external_id: impl Into<String>,
        content_hash: impl Into<String>,
        trust: TrustClass,
    ) -> Self {
        Self {
            kind: kind.into(),
            external_id: external_id.into(),
            content_hash: content_hash.into(),
            data_class: DataClass::Personal,
            trust,
            uri: None,
            captured_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRecord {
    pub id: SourceId,
    pub kind: String,
    pub external_id: String,
    pub revision: i32,
    pub content_hash: String,
    pub captured_at: DateTime<Utc>,
    pub data_class: DataClass,
    pub trust: TrustClass,
    pub uri: Option<String>,
    pub visibility: Visibility,
    pub deleted: bool,
}

/// Evidence for a memory, with enough source detail to inspect and export it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub source: SourceId,
    pub source_kind: String,
    pub external_id: String,
    pub revision: i32,
    pub uri: Option<String>,
    pub span: Option<String>,
    pub extraction_version: String,
    pub source_deleted: bool,
    pub source_hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub kind: String,
    pub status: MemoryStatus,
    pub content: String,
    pub topic_key: Option<String>,
    pub project: Option<String>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub inferred: bool,
    pub importance: i16,
    pub supersedes: Option<MemoryId>,
    pub invalidated_reason: Option<String>,
    pub accepted_by: String,
    pub evidence: Vec<EvidenceRecord>,
}

pub const DEFAULT_EXTRACTION_VERSION: &str = "v1";

/// A proposal plus optional metadata. `propose(MemoryCandidate)` uses the defaults.
#[derive(Debug, Clone)]
pub struct CandidateDraft {
    pub candidate: MemoryCandidate,
    pub topic: Option<String>,
    pub reason: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub extraction_version: String,
}

impl CandidateDraft {
    pub fn new(candidate: MemoryCandidate) -> Self {
        Self {
            candidate,
            topic: None,
            reason: None,
            observed_at: None,
            valid_from: None,
            valid_to: None,
            extraction_version: DEFAULT_EXTRACTION_VERSION.to_string(),
        }
    }
}
