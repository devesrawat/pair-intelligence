//! The library crates the API hosts, built once at startup (or by a test) and shared by handlers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pair_budget::PgBudget;
use pair_context::PairContextCompiler;
use pair_core::traits::Provider;
use pair_jobs::PgApprovals;
use pair_models::classification::pipeline::RoutingPipeline;
use pair_models::provider::store::ConversationStore;
use pair_models::provider::ProviderRegistry;
use pair_policy::PolicyEngine;
use tokio_util::task::TaskTracker;

use crate::adapter_budget::AdapterBudget;
use crate::attempts::AttemptStore;

/// Total wall-clock a turn may spend on provider attempts. Below the 30 s HTTP request timeout so
/// a slow provider ends the turn cleanly instead of being cut off between reserve and reconcile.
pub const DEFAULT_TURN_BUDGET: Duration = Duration::from_secs(25);

#[derive(Clone)]
pub struct Services {
    pub budget: Arc<PgBudget>,
    pub registry: Arc<ProviderRegistry>,
    pub provider: Arc<dyn Provider>,
    pub pipeline: Arc<RoutingPipeline>,
    pub compiler: Arc<PairContextCompiler>,
    pub store: ConversationStore,
    pub attempts: AttemptStore,
    pub policy: Arc<PolicyEngine>,
    pub approvals: PgApprovals,
    /// Workspace the policy engine resolves paths against. Server-side only: a caller-chosen root
    /// would let a request declare any directory (such as `/`) to be inside the workspace.
    pub workspace_root: Option<PathBuf>,
    /// Second credential for `POST /v1/approvals`; unset disables approval creation entirely.
    pub approver_token: Option<Arc<str>>,
    /// Same decision the provider adapters were built with (`PAIR_ALLOW_UNVERIFIED_MODEL_IDS`).
    pub allow_unverified_ids: bool,
    pub turn_budget: Duration,
    /// Server-side cost rules for the adapter routes `/v1/budget/*`.
    pub adapter_budget: AdapterBudget,
    /// Every spawned turn. A turn outlives its HTTP request, so shutdown waits on this tracker.
    pub turns: TaskTracker,
}
