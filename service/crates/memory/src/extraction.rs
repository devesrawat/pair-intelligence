//! Candidate extraction schema. Extractors (LLM or rule based) must emit this structure via
//! JSON mode / tool use; free text is never parsed. The `inferred` flag is mandatory so that
//! observations and inferences stay distinguishable through the whole lifecycle.
use crate::{
    error::invalid,
    inbox::Proposal,
    model::{validate_kind, CandidateDraft, DEFAULT_EXTRACTION_VERSION},
    store::PgMemory,
};
use chrono::{DateTime, Utc};
use pair_core::{
    error::Result,
    ids::SourceId,
    types::{EvidenceRef, MemoryCandidate},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_BATCH: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedEvidence {
    pub source: Uuid,
    pub span: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedCandidate {
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// True when the claim is concluded rather than stated or observed. Required.
    pub inferred: bool,
    /// Why this is worth remembering; shown in the inbox.
    pub reason: String,
    pub evidence: Vec<ExtractedEvidence>,
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub valid_to: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionBatch {
    pub extraction_version: String,
    pub candidates: Vec<ExtractedCandidate>,
}

pub fn parse_batch(json: &str) -> Result<ExtractionBatch> {
    let batch: ExtractionBatch =
        serde_json::from_str(json).map_err(|e| invalid(format!("extraction output does not match schema: {e}")))?;
    if batch.extraction_version.trim().is_empty() {
        return Err(invalid("extraction_version is required"));
    }
    if batch.candidates.len() > MAX_BATCH {
        return Err(invalid(format!("at most {MAX_BATCH} candidates per batch")));
    }
    for c in &batch.candidates {
        validate_kind(&c.kind)?;
        if c.content.trim().is_empty() || c.reason.trim().is_empty() {
            return Err(invalid("candidate content and reason must be non-empty"));
        }
    }
    Ok(batch)
}

/// JSON Schema to hand to the extractor (tool-use input schema).
pub fn extraction_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["extraction_version", "candidates"],
        "properties": {
            "extraction_version": {"type": "string"},
            "candidates": {"type": "array", "maxItems": MAX_BATCH, "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["kind", "content", "inferred", "reason", "evidence"],
                "properties": {
                    "kind": {"enum": crate::model::MEMORY_KINDS},
                    "content": {"type": "string"},
                    "topic": {"type": ["string", "null"]},
                    "project": {"type": ["string", "null"]},
                    "inferred": {"type": "boolean"},
                    "reason": {"type": "string"},
                    "evidence": {"type": "array", "items": {
                        "type": "object", "additionalProperties": false, "required": ["source"],
                        "properties": {"source": {"type": "string", "format": "uuid"}, "span": {"type": ["string", "null"]}}
                    }},
                    "observed_at": {"type": ["string", "null"], "format": "date-time"},
                    "valid_from": {"type": ["string", "null"], "format": "date-time"},
                    "valid_to": {"type": ["string", "null"], "format": "date-time"}
                }
            }}
        }
    })
}

impl ExtractedCandidate {
    pub fn into_draft(self, extraction_version: &str) -> CandidateDraft {
        let mut draft = CandidateDraft::new(MemoryCandidate {
            kind: self.kind,
            content: self.content,
            project: self.project,
            inferred: self.inferred,
            evidence: self
                .evidence
                .into_iter()
                .map(|e| EvidenceRef { source: SourceId(e.source), span: e.span })
                .collect(),
        });
        draft.topic = self.topic;
        draft.reason = Some(self.reason);
        draft.observed_at = self.observed_at;
        draft.valid_from = self.valid_from;
        draft.valid_to = self.valid_to;
        draft.extraction_version = if extraction_version.is_empty() {
            DEFAULT_EXTRACTION_VERSION.to_string()
        } else {
            extraction_version.to_string()
        };
        draft
    }
}

impl PgMemory {
    /// Propose every candidate in an extraction batch, in order.
    pub async fn propose_batch(&self, batch: ExtractionBatch) -> Result<Vec<Proposal>> {
        let mut out = Vec::with_capacity(batch.candidates.len());
        for candidate in batch.candidates {
            out.push(self.propose_with_outcome(candidate.into_draft(&batch.extraction_version)).await?);
        }
        Ok(out)
    }
}
