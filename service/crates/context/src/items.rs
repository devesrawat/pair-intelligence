//! Candidate context items and the trimming policy.
use crate::hashing::sha256_hex;
use crate::wrap::{wrap_external, wrap_external_attrs};
use crate::TokenCounter;
use pair_core::types::{EvidenceItem, EvidenceStatus, ManifestEntry, ModelMessage, TrustClass};

pub const SECTION_POLICY: &str = "policy";
pub const SECTION_MEMORY: &str = "memory";
pub const SECTION_CONVERSATION: &str = "conversation";
pub const SECTION_TOOL_RESULT: &str = "tool_result";
pub const SECTION_TASK: &str = "task";

pub const REASON_SECTION_BUDGET: &str = "section_budget_exceeded";
pub const REASON_CONTEXT_LIMIT: &str = "context_limit_exceeded";

const ROLE_USER: &str = "user";
const ROLE_ASSISTANT: &str = "assistant";

#[derive(Debug, Clone)]
pub struct Item {
    pub section: &'static str,
    pub id: String,
    pub hash: String,
    pub message: ModelMessage,
    pub tokens: u64,
    pub score: f64,
    pub omitted: Option<&'static str>,
}

impl Item {
    pub fn new(
        section: &'static str,
        id: String,
        original: &str,
        message: ModelMessage,
        score: f64,
        c: &dyn TokenCounter,
    ) -> Self {
        let tokens = c.count(&message.content);
        Self {
            section,
            id,
            hash: sha256_hex(original),
            message,
            tokens,
            score,
            omitted: None,
        }
    }

    pub fn kept(&self) -> bool {
        self.omitted.is_none()
    }

    pub fn entry(&self) -> ManifestEntry {
        ManifestEntry {
            section: self.section.to_string(),
            id: self.id.clone(),
            hash: self.hash.clone(),
            tokens: self.tokens,
            included: self.kept(),
            omitted_reason: self.omitted.map(str::to_string),
        }
    }
}

/// Status, supersession, conflicts and inference flag, as compiler-written header attributes.
fn memory_attrs(e: &EvidenceItem) -> Vec<(&'static str, String)> {
    let mut attrs = Vec::new();
    match e.status {
        EvidenceStatus::Current => {}
        EvidenceStatus::Superseded => attrs.push(("status", "superseded".to_string())),
        EvidenceStatus::Conflicting => attrs.push(("status", "conflicting".to_string())),
    }
    if let Some(by) = e.superseded_by {
        attrs.push(("superseded_by", by.to_string()));
    }
    if !e.conflicts_with.is_empty() {
        let ids: Vec<String> = e.conflicts_with.iter().map(ToString::to_string).collect();
        attrs.push(("conflicts_with", ids.join(":")));
    }
    if e.inferred {
        attrs.push(("inferred", "true".to_string()));
    }
    attrs
}

pub fn memory_item(index: usize, e: &EvidenceItem, c: &dyn TokenCounter) -> Item {
    let source = format!("memory:{}", e.memory.0);
    let content = wrap_external_attrs(&source, TrustClass::Untrusted, &memory_attrs(e), &e.content);
    let message = ModelMessage {
        role: ROLE_USER.into(),
        content,
        trust: TrustClass::Untrusted,
    };
    Item::new(
        SECTION_MEMORY,
        format!("{}#{index}", e.memory.0),
        &e.content,
        message,
        e.score,
        c,
    )
}

/// Only owner-authored user/assistant turns pass through raw; everything else is data.
pub fn conversation_item(index: usize, m: &ModelMessage, c: &dyn TokenCounter) -> Item {
    let passthrough =
        m.trust == TrustClass::Owner && (m.role == ROLE_USER || m.role == ROLE_ASSISTANT);
    let message = if passthrough {
        m.clone()
    } else {
        let content = wrap_external(&format!("conversation:{}", m.role), m.trust, &m.content);
        ModelMessage {
            role: ROLE_USER.into(),
            content,
            trust: m.trust,
        }
    };
    Item::new(
        SECTION_CONVERSATION,
        format!("recent:{index}"),
        &m.content,
        message,
        0.0,
        c,
    )
}

pub fn tool_item(index: usize, m: &ModelMessage, c: &dyn TokenCounter) -> Item {
    let content = wrap_external(&format!("tool:{}", m.role), m.trust, &m.content);
    let message = ModelMessage {
        role: ROLE_USER.into(),
        content,
        trust: m.trust,
    };
    Item::new(
        SECTION_TOOL_RESULT,
        format!("tool:{index}"),
        &m.content,
        message,
        0.0,
        c,
    )
}

pub fn kept_tokens(items: &[Item], section: &str) -> u64 {
    items
        .iter()
        .filter(|i| i.section == section && i.kept())
        .map(|i| i.tokens)
        .sum()
}

/// Index of the next item to drop within `section`: oldest first, except memories,
/// which drop lowest score first (later index first on ties).
fn next_victim(items: &[Item], section: &str) -> Option<usize> {
    let kept = items
        .iter()
        .enumerate()
        .filter(|(_, i)| i.section == section && i.kept());
    if section == SECTION_MEMORY {
        kept.min_by(|(ia, a), (ib, b)| a.score.total_cmp(&b.score).then(ib.cmp(ia)))
            .map(|(i, _)| i)
    } else {
        kept.map(|(i, _)| i).next()
    }
}

/// Drop items from `section` until it fits `ceiling`.
pub fn trim_section(items: &mut [Item], section: &str, ceiling: u64, reason: &'static str) {
    while kept_tokens(items, section) > ceiling {
        match next_victim(items, section) {
            Some(i) => items[i].omitted = Some(reason),
            None => break,
        }
    }
}

/// Global trim order: oldest conversation, lowest-score memory, then oldest tool result.
pub fn trim_global(items: &mut [Item], required: u64, limit: u64) {
    let total = |items: &[Item]| {
        required
            + items
                .iter()
                .filter(|i| i.kept())
                .map(|i| i.tokens)
                .sum::<u64>()
    };
    while total(items) > limit {
        let victim = [SECTION_CONVERSATION, SECTION_MEMORY, SECTION_TOOL_RESULT]
            .iter()
            .find_map(|s| next_victim(items, s));
        match victim {
            Some(i) => items[i].omitted = Some(REASON_CONTEXT_LIMIT),
            None => break,
        }
    }
}
