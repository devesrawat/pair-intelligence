//! Data types of the research workflow.
use chrono::{DateTime, NaiveDate, Utc};
use pair_core::ids::SourceId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchScope {
    pub question: String,
    /// Extra owner-authored queries; when empty the question itself is the query.
    #[serde(default)]
    pub queries: Vec<String>,
    pub max_sources: usize,
}

/// A captured source version. Its text is `TrustClass::Untrusted` data, never instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub url: String,
    pub normalized_url: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub revision: Option<String>,
    pub content_sha256: Option<String>,
    pub published_at: Option<NaiveDate>,
    pub fetched_at: DateTime<Utc>,
    pub text: Option<String>,
    pub duplicate_of: Option<SourceId>,
}

/// A claim as proposed by the extractor, before validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawClaim {
    pub topic: String,
    pub text: String,
    /// The key value asserted (number, date, name), verbatim from the span.
    pub value: String,
    pub url: String,
    /// Exact supporting text copied from the source.
    pub span: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    InventedUrl,
    SourceUnavailable,
    SpanNotInSource,
    Unsupported(String),
    JudgeRejected(String),
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InventedUrl => write!(f, "cited URL is not among captured sources"),
            Self::SourceUnavailable => write!(f, "cited source was inaccessible"),
            Self::SpanNotInSource => write!(f, "supporting span not found in captured source text"),
            Self::Unsupported(why) => write!(f, "source text does not support the claim: {why}"),
            Self::JudgeRejected(why) => write!(f, "support judge rejected the citation: {why}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: Uuid,
    pub raw: RawClaim,
    pub source: Option<SourceId>,
    pub span_start: Option<usize>,
    pub rejected: Option<RejectReason>,
}

impl Claim {
    pub fn is_valid(&self) -> bool {
        self.rejected.is_none()
    }
}

/// One topic where validated sources assert different values. Never averaged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub topic: String,
    /// (normalised value, claim ids asserting it)
    pub positions: Vec<(String, Vec<Uuid>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Statement {
    pub text: String,
    pub claim_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedStatement {
    pub text: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub run_id: Uuid,
    pub question: String,
    pub generated_at: DateTime<Utc>,
    pub sources: Vec<Source>,
    pub claims: Vec<Claim>,
    pub conflicts: Vec<Conflict>,
    pub statements: Vec<Statement>,
    pub rejected_statements: Vec<RejectedStatement>,
    pub limitations: Vec<String>,
}
