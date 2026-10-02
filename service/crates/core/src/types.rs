use crate::ids::*;
use crate::money::Micros;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    Public,
    Personal,
    Sensitive,
    Employer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    Owner,
    Tool,
    Untrusted,
}

// ---- Policy ----
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRequest {
    pub tool: String,
    pub executable: Option<String>,
    pub args: Vec<String>,
    pub paths: Vec<String>,
    pub destination: Option<String>,
    pub data_class: DataClass,
    pub task: TaskId,
    pub trace: TraceId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyContext {
    pub workspace_root: String,
    pub approvals: Vec<ApprovalId>,
    pub policy_version: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny { reason: String },
    NeedsApproval { payload_hash: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyOutcome {
    pub decision: Decision,
    pub policy_version: String,
}

// ---- Budget ----
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub actual_cost: Option<Micros>,
    pub price_version: String,
}
/// Per-task cap selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Default,
    Research,
    Coding,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Research => "research",
            Self::Coding => "coding",
        }
    }
}

/// `Classifier` spend also counts toward day/month totals but has its own monthly sub-cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetCategory {
    Metered,
    Classifier,
}

impl BudgetCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metered => "metered",
            Self::Classifier => "classifier",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveRequest {
    pub task: TaskId,
    pub max_cost: Micros,
    pub kind: TaskKind,
    pub category: BudgetCategory,
    /// `None` selects the budget's current price version.
    pub price_version: Option<String>,
}

impl ReserveRequest {
    pub fn metered(task: TaskId, max_cost: Micros, kind: TaskKind, price_version: String) -> Self {
        Self {
            task,
            max_cost,
            kind,
            category: BudgetCategory::Metered,
            price_version: Some(price_version),
        }
    }

    pub fn classifier(task: TaskId, max_cost: Micros, price_version: String) -> Self {
        Self {
            task,
            max_cost,
            kind: TaskKind::Default,
            category: BudgetCategory::Classifier,
            price_version: Some(price_version),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: LedgerEntryId,
    pub reservation: ReservationId,
    pub amount: Micros,
    pub settled: bool,
}

// ---- Models / classification ----
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: String,
    pub content: String,
    pub trust: TrustClass,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model_id: String,
    pub messages: Vec<ModelMessage>,
    pub max_output_tokens: u32,
    pub deadline_ms: u64,
    pub data_class: DataClass,
    pub task: TaskId,
    pub trace: TraceId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub resolved_model: String,
    pub text: String,
    pub usage: UsageReport,
    pub provider_request_id: Option<String>,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationInput {
    pub request: String,
    pub recent_summary: String,
    pub project_type: Option<String>,
    pub workflows: Vec<String>,
    pub task: TaskId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskClassification {
    pub model_version: String,
    pub question_version: String,
    pub intent: String,
    pub difficulty: String,
    pub intent_probabilities: BTreeMap<String, f64>,
    pub difficulty_probabilities: BTreeMap<String, f64>,
    pub intent_confidence: f64,
    pub difficulty_confidence: f64,
    pub input_tokens: u64,
    pub latency_ms: u64,
    pub request_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProfile {
    pub intent: String,
    pub difficulty: String,
    pub data_class: DataClass,
    pub needs_tools: bool,
    pub est_input_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecision {
    pub model_id: String,
    pub reason: String,
    pub classifier_mode: String,
}

// ---- Memory / context ----
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub source: SourceId,
    pub span: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub kind: String,
    pub content: String,
    pub project: Option<String>,
    pub inferred: bool,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalQuery {
    pub text: String,
    pub project: Option<String>,
    pub as_of: DateTime<Utc>,
    pub limit: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceItem {
    pub memory: MemoryId,
    pub content: String,
    pub evidence: Vec<EvidenceRef>,
    pub score: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLimits {
    pub context_tokens: u64,
    pub max_output_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContext {
    pub objective: String,
    pub output_contract: String,
    pub policy_summary: String,
    pub recent: Vec<ModelMessage>,
    pub tool_results: Vec<ModelMessage>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledContext {
    pub messages: Vec<ModelMessage>,
    pub manifest: Vec<ManifestEntry>,
    pub total_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub section: String,
    pub id: String,
    pub hash: String,
    pub tokens: u64,
    pub included: bool,
    pub omitted_reason: Option<String>,
}

// ---- Workflow / approval ----
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    WaitingApproval,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInput {
    pub kind: String,
    pub payload: serde_json::Value,
}
