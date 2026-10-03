//! Reads the configuration files and credentials named by [`Config`] into [`Services`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pair_budget::BudgetConfig;
use pair_context::ContextConfig;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::traits::Classifier;
use pair_models::classification::config::{ClassifierMode, RoutingConfig};
use pair_models::classification::jev::{ApiKey, JevClassifier, JevSettings};
use pair_models::classification::questions::QuestionSet;
use pair_models::provider::{
    AnthropicProvider, CloudProvider, OllamaCloudProvider, ProviderRegistry,
};
use pair_policy::PolicyEngine;
use sqlx::PgPool;

use crate::config::Config;
use crate::services::Services;
use crate::wiring::{build_services, ServiceInputs};

fn invalid(what: &str, e: impl std::fmt::Display) -> PairError {
    PairError::new(ErrorCode::InvalidInput, format!("{what}: {e}"))
}

fn read(path: &Path, what: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| invalid(&format!("read {what} {}", path.display()), e))
}

/// `questions_path` in models.yaml is written relative to the repository (or image) root, which is
/// the parent of the directory holding models.yaml.
fn questions_file(models_config: &Path, questions_path: &str) -> PathBuf {
    let p = Path::new(questions_path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let root = models_config
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new(""));
    root.join(p)
}

fn classifier(
    cfg: &Config,
    routing: &RoutingConfig,
    questions: &QuestionSet,
) -> Result<Option<Arc<dyn Classifier>>> {
    if routing.classifier.mode == ClassifierMode::Disabled {
        return Ok(None);
    }
    let Some(key) = &cfg.typesafe_api_key else {
        tracing::info!("TYPESAFE_API_KEY is not set: no classifier, every turn uses the baseline");
        return Ok(None);
    };
    let settings = JevSettings {
        endpoint: routing.classifier.endpoint.clone(),
        model: routing.classifier.model.clone(),
        deadline: Duration::from_millis(routing.classifier.deadline_ms),
    };
    let jev = JevClassifier::new(settings, ApiKey::new(key.expose()), questions.clone())?;
    Ok(Some(Arc::new(jev)))
}

fn provider(cfg: &Config, registry: &Arc<ProviderRegistry>) -> Result<CloudProvider> {
    let allow = cfg.allow_unverified_model_ids;
    let anthropic = cfg
        .anthropic_api_key
        .clone()
        .map(|k| {
            AnthropicProvider::new(k, registry.clone()).map(|p| p.with_allow_unverified_ids(allow))
        })
        .transpose()?;
    let ollama = cfg
        .ollama_api_key
        .clone()
        .map(|k| {
            OllamaCloudProvider::new(k, registry.clone())
                .map(|p| p.with_allow_unverified_ids(allow))
        })
        .transpose()?;
    if anthropic.is_none() && ollama.is_none() {
        tracing::warn!("no provider API key configured: every model call will fail closed");
    }
    Ok(CloudProvider::new(registry.clone(), anthropic, ollama))
}

/// `Ok(None)`: a default config path does not exist, so the services are left unconfigured
/// (`/v1/*` answers 503 and `/readyz` is not ready). An explicitly configured path that is missing
/// never gets here: `Config::validate_paths` already refused to start. A file that exists but is
/// invalid is an error.
pub fn load_services(
    cfg: &Config,
    pool: PgPool,
    registry: Option<Arc<ProviderRegistry>>,
) -> Result<Option<Services>> {
    let Some(registry) = registry else {
        tracing::warn!(
            "provider registry not loaded: budget, policy and routing services unavailable"
        );
        return Ok(None);
    };
    for (name, path) in [
        ("budget", &cfg.budget_config),
        ("policy", &cfg.policy_config),
        ("context", &cfg.context_config),
    ] {
        if !path.is_file() {
            tracing::warn!(config = name, path = %path.display(), "config file not found: services unavailable");
            return Ok(None);
        }
    }
    let home = cfg.home.clone().ok_or_else(|| {
        PairError::new(
            ErrorCode::InvalidInput,
            "HOME must be set for the policy engine",
        )
    })?;
    let routing = RoutingConfig::from_path(&cfg.models_config)?;
    let questions = QuestionSet::from_path(&questions_file(
        &cfg.models_config,
        &routing.classifier.questions_path,
    ))?;
    let context =
        ContextConfig::parse(&read(&cfg.context_config, "context config")?)?.default_budgets()?;
    let policy = PolicyEngine::from_config_file(&cfg.policy_config, &home)
        .map_err(|e| invalid("policy config", e))?;
    let mut services = build_services(ServiceInputs {
        pool,
        budget: BudgetConfig::load(&cfg.budget_config)?,
        provider: Arc::new(provider(cfg, &registry)?),
        classifier: classifier(cfg, &routing, &questions)?,
        registry,
        routing,
        questions,
        context,
        policy,
        workspace_root: cfg.workspace_root.clone(),
        approver_token: cfg.approver_token.as_ref().map(|t| t.expose().to_owned()),
        allow_unverified_ids: cfg.allow_unverified_model_ids,
    })?;
    services.turn_budget = cfg.turn_budget;
    services.adapter_budget = cfg.adapter_budget;
    Ok(Some(services))
}
