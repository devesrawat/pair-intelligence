//! The logical interfaces of spec section 15. Implemented by sibling crates.
use crate::{error::Result, ids::*, money::Micros, types::*};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[async_trait]
pub trait Classifier: Send + Sync {
    async fn classify(&self, input: ClassificationInput) -> Result<TaskClassification>;
}
#[async_trait]
pub trait Provider: Send + Sync {
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse>;
}
pub trait Policy: Send + Sync {
    fn authorize(&self, req: &ActionRequest, ctx: &PolicyContext) -> PolicyOutcome;
}
#[async_trait]
pub trait Budget: Send + Sync {
    async fn reserve(&self, task: TaskId, max_cost: Micros) -> Result<ReservationId>;
    async fn reconcile(&self, id: ReservationId, usage: UsageReport) -> Result<LedgerEntry>;
}
#[async_trait]
pub trait Memory: Send + Sync {
    async fn propose(&self, c: MemoryCandidate) -> Result<CandidateId>;
    async fn accept(&self, id: CandidateId, actor: &str) -> Result<MemoryId>;
    async fn retrieve(&self, q: RetrievalQuery) -> Result<Vec<EvidenceItem>>;
}
pub trait ContextCompiler: Send + Sync {
    fn compile(&self, ctx: &TaskContext, limits: &ModelLimits, memory: &[EvidenceItem]) -> Result<CompiledContext>;
}
pub trait Router: Send + Sync {
    fn select(&self, profile: &TaskProfile, classification: Option<&TaskClassification>) -> Result<RouteDecision>;
}
#[async_trait]
pub trait Workflows: Send + Sync {
    async fn start(&self, input: WorkflowInput, key: IdempotencyKey) -> Result<RunId>;
    async fn resume(&self, id: RunId) -> Result<RunState>;
}
#[async_trait]
pub trait Approvals: Send + Sync {
    async fn approve(&self, action_hash: &str, actor: &str, expiry: DateTime<Utc>) -> Result<ApprovalId>;
    /// Checked and consumed at execution; single use.
    async fn consume(&self, id: ApprovalId, action_hash: &str) -> Result<()>;
}
