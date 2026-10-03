//! A complete in-process service: real Postgres, real `PgBudget`, real conversation store, real
//! policy engine from `config/policy.yaml`, with only the provider and Jev replaced by loopback mocks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use pair_api::providers::ProviderHealth;
use pair_api::services::Services;
use pair_api::state::AppState;
use pair_api::wiring::{build_services, ServiceInputs};
use pair_budget::BudgetConfig;
use pair_context::ContextConfig;
use pair_core::money::{Micros, Price};
use pair_core::traits::Classifier;
use pair_core::types::DataClass;
use pair_models::classification::config::RoutingConfig;
use pair_models::classification::jev::{ApiKey, JevClassifier, JevSettings};
use pair_models::classification::questions::QuestionSet;
use pair_models::provider::{
    AnthropicProvider, CloudProvider, Endpoint, Health, ModelEntry, ProviderKind, ProviderRegistry,
};
use pair_policy::PolicyEngine;
use pair_telemetry::health::{DiskStats, BYTES_PER_GIB};
use pair_telemetry::Secret;

use super::mock::{spawn_jev, spawn_provider, JevMode, Mock, ProviderMode};
use super::TestDb;

pub const PRICE_VERSION: &str = "pv-test";
pub const CHEAP: &str = "cheap-model";
pub const MID: &str = "mid-model";
pub const PREMIUM: &str = "premium-model";
pub const APPROVER_TOKEN: &str = "approver-token-0123456789";
const TURN_BUDGET: Duration = Duration::from_secs(10);

pub struct StackOpts {
    pub budget_yaml: String,
    pub unverified: Vec<&'static str>,
    pub provider: ProviderMode,
    pub jev: Option<JevMode>,
    /// Tier the router starts from for every intent (`routine` = cheap first, `strong` = mid first).
    pub baseline_tier: &'static str,
    pub max_attempts: usize,
}

impl Default for StackOpts {
    fn default() -> Self {
        Self {
            budget_yaml: budget_yaml("20.00", "1.00", "1.00", "0.10"),
            unverified: Vec::new(),
            provider: ProviderMode::ok("hello from the mock"),
            jev: None,
            baseline_tier: "strong",
            max_attempts: 3,
        }
    }
}

pub fn budget_yaml(month: &str, day: &str, classifier: &str, task: &str) -> String {
    let coding = if task == "0.00" { "0.00" } else { "1.00" };
    format!(
        "budget:\n  currency: USD\n  metered_monthly_cap: {month}\n  metered_daily_cap: {day}\n  \
         classifier_monthly_subcap: {classifier}\n  default_task_cap: {task}\n  \
         research_task_cap: {task}\n  coding_task_cap: {coding}\n  auto_top_up: false\n\
         schedule:\n  timezone: Asia/Kolkata\n"
    )
}

pub struct Stack {
    pub db: TestDb,
    pub app: Router,
    pub provider: Mock,
    pub jev: Option<Mock>,
    pub services: Services,
    pub workspace: PathBuf,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn entry(id: &str, base: &str, in_micros: i64, out_micros: i64, verified: bool) -> ModelEntry {
    ModelEntry {
        id: id.to_owned(),
        provider: ProviderKind::Anthropic,
        endpoint: Endpoint::unchecked_for_tests(base),
        modalities: vec!["text".to_owned()],
        context_tokens: 200_000,
        max_output_tokens: 8_192,
        tools: false,
        structured_output: false,
        price: Some(Price {
            version: PRICE_VERSION.to_owned(),
            input_per_mtok: Micros(in_micros),
            output_per_mtok: Micros(out_micros),
        }),
        data_policy: "test".to_owned(),
        allowed_data_classes: vec![DataClass::Public, DataClass::Personal],
        quota_requests_per_minute: None,
        health: Health::Healthy,
        id_verified: verified,
    }
}

fn routing_yaml(baseline: &str, max_attempts: usize) -> String {
    let by_intent: String = [
        "coding",
        "research",
        "planning",
        "memory_recall",
        "transformation",
        "mixed",
        "uncertain",
    ]
    .iter()
    .map(|i| format!("{i}: {baseline}"))
    .collect::<Vec<_>>()
    .join(", ");
    format!(
        "routing:\n  classifier:\n    mode: shadow\n    endpoint: https://api.typesafe.ai/v1/systemone\n    \
         model: jev-1.13.0\n    deadline_ms: 1000\n    input_price_micros_per_mtok: 42000\n    \
         price_version: jev-test\n    questions_path: config/jev-questions.json\n    \
         allowed_data_classes: [public, personal]\n    active_categories: []\n  \
         max_model_attempts: {max_attempts}\n  task_cap_micros: 100000\n  max_output_tokens: 4096\n  \
         tier_order: [routine, strong, deep]\n  \
         tier_by_difficulty: {{routine: routine, substantial: strong, deep: deep}}\n  \
         baseline:\n    default_tier: {baseline}\n    by_intent: {{{by_intent}}}\n  thresholds: []\n  \
         data_class_providers:\n    public: [anthropic]\n    personal: [anthropic]\n  candidates:\n    \
         - {{id: {CHEAP}, provider: anthropic, tier: routine, capabilities: [text], context_tokens: 200000, available: true, input_price_micros_per_mtok: 1000000, output_price_micros_per_mtok: 5000000}}\n    \
         - {{id: {MID}, provider: anthropic, tier: strong, capabilities: [text], context_tokens: 200000, available: true, input_price_micros_per_mtok: 2000000, output_price_micros_per_mtok: 10000000}}\n    \
         - {{id: {PREMIUM}, provider: anthropic, tier: deep, capabilities: [text], context_tokens: 200000, available: true, input_price_micros_per_mtok: 4000000, output_price_micros_per_mtok: 20000000}}\n"
    )
}

fn roomy_disk() -> pair_api::state::DiskProbe {
    Arc::new(|_| {
        Ok(DiskStats {
            total_bytes: 100 * BYTES_PER_GIB,
            free_bytes: 80 * BYTES_PER_GIB,
        })
    })
}

impl Stack {
    pub async fn start(opts: StackOpts) -> Self {
        let db = TestDb::create_migrated().await;
        let provider = spawn_provider(opts.provider.clone()).await;
        let jev = match &opts.jev {
            Some(mode) => Some(spawn_jev(mode.clone()).await),
            None => None,
        };
        let verified = |id: &str| !opts.unverified.contains(&id);
        let registry = Arc::new(
            ProviderRegistry::from_entries(vec![
                entry(CHEAP, &provider.base, 1_000_000, 5_000_000, verified(CHEAP)),
                entry(MID, &provider.base, 2_000_000, 10_000_000, verified(MID)),
                entry(
                    PREMIUM,
                    &provider.base,
                    4_000_000,
                    20_000_000,
                    verified(PREMIUM),
                ),
            ])
            .expect("registry"),
        );
        let adapter = AnthropicProvider::new(Secret::new("mock-key-not-real"), registry.clone())
            .expect("anthropic adapter")
            .with_allow_unverified_ids(false);
        let questions = QuestionSet::from_path(&repo_root().join("config/jev-questions.json"))
            .expect("questions");
        let classifier: Option<Arc<dyn Classifier>> = jev.as_ref().map(|m| {
            let settings = JevSettings {
                endpoint: m.base.clone(),
                model: "jev-1.13.0".to_owned(),
                deadline: Duration::from_secs(1),
            };
            Arc::new(
                JevClassifier::new(settings, ApiKey::new("mock-jev-key"), questions.clone())
                    .expect("jev"),
            ) as Arc<dyn Classifier>
        });
        let context = ContextConfig::parse(
            &std::fs::read_to_string(repo_root().join("config/context.yaml")).expect("context"),
        )
        .expect("context config")
        .default_budgets()
        .expect("budgets");
        let scratch =
            std::env::temp_dir().join(format!("pair_api_ws_{}", uuid::Uuid::new_v4().simple()));
        let workspace = scratch.join("workspace");
        let home = scratch.join("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&home).expect("home");
        let policy = PolicyEngine::from_config_file(&repo_root().join("config/policy.yaml"), &home)
            .expect("policy");
        let mut services = build_services(ServiceInputs {
            pool: db.pool.clone(),
            budget: BudgetConfig::from_yaml(&opts.budget_yaml).expect("budget"),
            registry: registry.clone(),
            routing: RoutingConfig::from_yaml(&routing_yaml(opts.baseline_tier, opts.max_attempts))
                .expect("routing"),
            classifier,
            questions,
            context,
            policy,
            provider: Arc::new(CloudProvider::new(registry.clone(), Some(adapter), None)),
            workspace_root: Some(workspace.clone()),
            approver_token: Some(APPROVER_TOKEN.to_owned()),
            allow_unverified_ids: false,
        })
        .expect("services");
        services.turn_budget = TURN_BUDGET;
        let state = AppState::new(db.pool.clone(), super::TOKEN)
            .with_services(services.clone())
            .with_providers(registry as Arc<dyn ProviderHealth>)
            .with_disk_probe(roomy_disk());
        Self {
            app: pair_api::router(state),
            db,
            provider,
            jev,
            services,
            workspace,
        }
    }

    pub async fn finish(self) {
        self.db.drop_db().await;
    }
}
