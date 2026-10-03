//! Builds [`Services`] from already-loaded inputs. The binary reads files and the environment into
//! [`ServiceInputs`]; tests hand in their own registry, routing and budget YAML.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use pair_budget::{BudgetConfig, PgBudget, PriceBook};
use pair_context::{ContextBudgets, PairContextCompiler};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::traits::{Classifier, Provider};
use pair_jobs::PgApprovals;
use pair_models::classification::config::RoutingConfig;
use pair_models::classification::pipeline::RoutingPipeline;
use pair_models::classification::questions::QuestionSet;
use pair_models::provider::store::ConversationStore;
use pair_models::provider::ProviderRegistry;
use pair_models::router::ConfigRouter;
use pair_policy::PolicyEngine;
use pair_workflows::calls::MAX_MODEL_ATTEMPTS;
use sqlx::PgPool;
use tokio_util::task::TaskTracker;

use crate::adapter_budget::AdapterBudget;
use crate::attempts::AttemptStore;
use crate::services::{Services, DEFAULT_TURN_BUDGET};

pub struct ServiceInputs {
    pub pool: PgPool,
    pub budget: BudgetConfig,
    pub registry: Arc<ProviderRegistry>,
    pub routing: RoutingConfig,
    pub classifier: Option<Arc<dyn Classifier>>,
    pub questions: QuestionSet,
    pub context: ContextBudgets,
    pub policy: PolicyEngine,
    pub provider: Arc<dyn Provider>,
    pub workspace_root: Option<PathBuf>,
    pub approver_token: Option<String>,
    pub allow_unverified_ids: bool,
}

/// Every routing candidate must be a priced registry model, or the router could plan a model the
/// workflows then refuse (or, worse, one whose price differs from the registry's).
fn check_candidates(routing: &RoutingConfig, registry: &ProviderRegistry) -> Result<()> {
    for c in &routing.candidates {
        let entry = registry.get(&c.id).ok_or_else(|| {
            PairError::new(
                ErrorCode::InvalidInput,
                format!("routing candidate {} is not in the provider registry", c.id),
            )
        })?;
        if entry.price.is_none() {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("routing candidate {} has no price in the registry", c.id),
            ));
        }
    }
    Ok(())
}

fn known_price_versions(registry: &ProviderRegistry, routing: &RoutingConfig) -> BTreeSet<String> {
    let mut versions: BTreeSet<String> = registry
        .iter()
        .filter_map(|e| e.price.as_ref().map(|p| p.version.clone()))
        .collect();
    versions.insert(routing.classifier.price_version.clone());
    versions
}

pub fn build_services(i: ServiceInputs) -> Result<Services> {
    check_candidates(&i.routing, &i.registry)?;
    let prices = PriceBook::new(None, known_price_versions(&i.registry, &i.routing));
    let max_attempts =
        u32::try_from(i.routing.max_model_attempts.min(MAX_MODEL_ATTEMPTS)).unwrap_or(u32::MAX);
    let pipeline = RoutingPipeline::new(ConfigRouter::new(i.routing), i.classifier, i.questions);
    Ok(Services {
        budget: Arc::new(PgBudget::new(i.pool.clone(), i.budget, prices)),
        registry: i.registry,
        provider: i.provider,
        pipeline: Arc::new(pipeline),
        compiler: Arc::new(PairContextCompiler::new(i.context)),
        store: ConversationStore::new(i.pool.clone()),
        attempts: AttemptStore::new(i.pool.clone(), max_attempts),
        policy: Arc::new(i.policy),
        approvals: PgApprovals::new(i.pool),
        workspace_root: i.workspace_root,
        approver_token: i.approver_token.map(Arc::from),
        allow_unverified_ids: i.allow_unverified_ids,
        turn_budget: DEFAULT_TURN_BUDGET,
        adapter_budget: AdapterBudget::default(),
        allow_turn_kind_override: false,
        turns: TaskTracker::new(),
    })
}
