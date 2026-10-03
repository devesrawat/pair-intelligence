//! Provider registry loaded from `config/models.yaml` (spec section 5).
use super::guard::Endpoint;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::money::{Micros, Price};
use pair_core::types::{DataClass, ModelLimits};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    OllamaCloud,
    /// Claude served through the `claude` CLI on the PAIR host (subscription login).
    ClaudeCode,
}

/// How a model is paid for. Subscription calls have zero marginal cost, so the dollar budget
/// cannot bound them; they are bounded by a mandatory per-minute request quota instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    #[default]
    Metered,
    Subscription,
}

/// Price version recorded on usage for subscription-billed calls.
pub const SUBSCRIPTION_PRICE_VERSION: &str = "subscription";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    #[default]
    Healthy,
    Degraded,
    Disabled,
}

#[derive(Debug, Clone, Deserialize)]
struct PriceConfig {
    version: String,
    input_per_mtok_micros: i64,
    output_per_mtok_micros: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct EntryConfig {
    id: String,
    provider: ProviderKind,
    /// Id sent to the provider when it differs from the registry id (e.g. a CLI alias).
    #[serde(default)]
    upstream_id: Option<String>,
    #[serde(default)]
    billing: Billing,
    endpoint: String,
    #[serde(default = "default_modalities")]
    modalities: Vec<String>,
    context_tokens: u64,
    max_output_tokens: u64,
    #[serde(default)]
    tools: bool,
    #[serde(default)]
    structured_output: bool,
    /// Absent means unknown price: paid execution is disabled.
    price: Option<PriceConfig>,
    data_policy: String,
    allowed_data_classes: Vec<DataClass>,
    #[serde(default)]
    quota_requests_per_minute: Option<u32>,
    #[serde(default)]
    health: Health,
    /// False when the model id has not been confirmed against the provider catalog.
    #[serde(default = "default_id_verified")]
    id_verified: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryConfig {
    models: Vec<EntryConfig>,
}

fn default_id_verified() -> bool {
    true
}

fn default_modalities() -> Vec<String> {
    vec!["text".to_owned()]
}

/// One model the service may call.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub provider: ProviderKind,
    /// Id sent to the provider; equals `id` unless the registry sets `upstream_id`.
    pub upstream_id: String,
    pub billing: Billing,
    pub endpoint: Endpoint,
    pub modalities: Vec<String>,
    pub context_tokens: u64,
    pub max_output_tokens: u64,
    pub tools: bool,
    pub structured_output: bool,
    pub price: Option<Price>,
    pub data_policy: String,
    pub allowed_data_classes: Vec<DataClass>,
    pub quota_requests_per_minute: Option<u32>,
    pub health: Health,
    /// False means the id is a guess; `generate` refuses unless explicitly overridden.
    pub id_verified: bool,
}

impl ModelEntry {
    /// Unknown price means automatic paid execution is disabled.
    pub fn paid_execution_enabled(&self) -> bool {
        self.price.is_some()
    }

    pub fn limits(&self) -> ModelLimits {
        ModelLimits {
            context_tokens: self.context_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }

    pub fn allows(&self, class: DataClass) -> bool {
        self.allowed_data_classes.contains(&class)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    entries: BTreeMap<String, ModelEntry>,
}

impl ProviderRegistry {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            PairError::new(
                ErrorCode::InvalidInput,
                format!("read {}: {e}", path.display()),
            )
        })?;
        let registry = Self::from_yaml_str(&text)?;
        registry.warn_unverified();
        Ok(registry)
    }

    /// Ids whose catalog verification is still pending.
    pub fn unverified_ids(&self) -> Vec<String> {
        self.iter()
            .filter(|e| !e.id_verified)
            .map(|e| e.id.clone())
            .collect()
    }

    /// Startup warning for every unverified model id.
    pub fn warn_unverified(&self) {
        for id in self.unverified_ids() {
            tracing::warn!(
                model = %id,
                "model id is unverified (id_verified: false); calls are refused unless {}=1",
                super::common::ALLOW_UNVERIFIED_ENV
            );
        }
    }

    /// Parse and validate. Every endpoint passes the cloud-only guard.
    pub fn from_yaml_str(yaml: &str) -> Result<Self> {
        let cfg: RegistryConfig = serde_yaml_ng::from_str(yaml)
            .map_err(|e| PairError::new(ErrorCode::InvalidInput, format!("models.yaml: {e}")))?;
        let mut entries = Vec::with_capacity(cfg.models.len());
        for e in cfg.models {
            let endpoint = Endpoint::parse(&e.endpoint)?;
            entries.push(build_entry(e, endpoint)?);
        }
        Self::from_entries(entries)
    }

    /// Build from prepared entries (used by mock-backed tests with unchecked endpoints).
    pub fn from_entries(entries: Vec<ModelEntry>) -> Result<Self> {
        let mut map = BTreeMap::new();
        for e in entries {
            if map.insert(e.id.clone(), e.clone()).is_some() {
                return Err(PairError::new(
                    ErrorCode::InvalidInput,
                    format!("duplicate model id {}", e.id),
                ));
            }
        }
        Ok(Self { entries: map })
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.entries.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModelEntry> {
        self.entries.values()
    }

    /// Returns a copy with one entry's health replaced.
    #[must_use]
    pub fn with_health(&self, id: &str, health: Health) -> Self {
        let mut entries = self.entries.clone();
        if let Some(e) = entries.get_mut(id) {
            e.health = health;
        }
        Self { entries }
    }
}

fn build_entry(e: EntryConfig, endpoint: Endpoint) -> Result<ModelEntry> {
    if e.context_tokens == 0 || e.max_output_tokens == 0 || e.max_output_tokens > e.context_tokens {
        return Err(PairError::new(
            ErrorCode::InvalidInput,
            format!("model {}: invalid token limits", e.id),
        ));
    }
    if e.billing == Billing::Subscription {
        if e.provider == ProviderKind::Anthropic {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!(
                    "model {}: the Anthropic API is always metered; use provider claude_code for a subscription",
                    e.id
                ),
            ));
        }
        if e.price.is_some() {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("model {}: subscription billing takes no price", e.id),
            ));
        }
        if e.quota_requests_per_minute == Some(0) {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!(
                    "model {}: quota_requests_per_minute must be > 0 when set",
                    e.id
                ),
            ));
        }
    }
    if e.billing == Billing::Metered
        && e.price
            .as_ref()
            .is_some_and(|p| p.input_per_mtok_micros == 0 && p.output_per_mtok_micros == 0)
    {
        return Err(PairError::new(
            ErrorCode::InvalidInput,
            format!(
                "model {}: a zero price on a metered entry would bypass the budget; use billing: subscription",
                e.id
            ),
        ));
    }
    let price = match e.price {
        _ if e.billing == Billing::Subscription => Some(Price {
            version: SUBSCRIPTION_PRICE_VERSION.to_owned(),
            input_per_mtok: Micros(0),
            output_per_mtok: Micros(0),
        }),
        Some(p) if p.input_per_mtok_micros < 0 || p.output_per_mtok_micros < 0 => {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("model {}: negative price", e.id),
            ));
        }
        Some(p) => Some(Price {
            version: p.version,
            input_per_mtok: Micros(p.input_per_mtok_micros),
            output_per_mtok: Micros(p.output_per_mtok_micros),
        }),
        None => None,
    };
    Ok(ModelEntry {
        upstream_id: e.upstream_id.unwrap_or_else(|| e.id.clone()),
        billing: e.billing,
        id: e.id,
        provider: e.provider,
        endpoint,
        modalities: e.modalities,
        context_tokens: e.context_tokens,
        max_output_tokens: e.max_output_tokens,
        tools: e.tools,
        structured_output: e.structured_output,
        price,
        data_policy: e.data_policy,
        allowed_data_classes: e.allowed_data_classes,
        quota_requests_per_minute: e.quota_requests_per_minute,
        health: e.health,
        id_verified: e.id_verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
models:
  - id: m-priced
    provider: anthropic
    endpoint: https://api.anthropic.com
    context_tokens: 200000
    max_output_tokens: 8192
    price: { version: v1, input_per_mtok_micros: 2000000, output_per_mtok_micros: 10000000 }
    data_policy: retention_30d_no_training
    allowed_data_classes: [public, personal]
  - id: m-unpriced
    provider: ollama_cloud
    endpoint: https://ollama.com
    context_tokens: 128000
    max_output_tokens: 8192
    data_policy: no_logging
    allowed_data_classes: [public]
"#;

    #[test]
    fn registry_unknown_price_disables_paid_execution() {
        let r = ProviderRegistry::from_yaml_str(YAML).expect("parse");
        assert!(r.get("m-priced").expect("entry").paid_execution_enabled());
        assert!(!r.get("m-unpriced").expect("entry").paid_execution_enabled());
    }

    const SUB_YAML: &str = r#"
models:
  - id: claude-code/sonnet
    provider: claude_code
    upstream_id: sonnet
    billing: subscription
    endpoint: https://api.anthropic.com
    context_tokens: 200000
    max_output_tokens: 16000
    quota_requests_per_minute: 10
    data_policy: subscription_terms
    allowed_data_classes: [public, personal]
"#;

    #[test]
    fn registry_subscription_entry_is_zero_cost_and_enabled() {
        let r = ProviderRegistry::from_yaml_str(SUB_YAML).expect("parse");
        let e = r.get("claude-code/sonnet").expect("entry");
        assert_eq!(e.provider, ProviderKind::ClaudeCode);
        assert_eq!(e.billing, Billing::Subscription);
        assert_eq!(e.upstream_id, "sonnet");
        assert!(e.paid_execution_enabled());
        let price = e.price.as_ref().expect("synthetic zero price");
        assert_eq!(price.version, SUBSCRIPTION_PRICE_VERSION);
        assert_eq!(price.max_cost(1_000_000, 16_000), Some(Micros(0)));
    }

    #[test]
    fn registry_subscription_without_quota_is_accepted() {
        let ok = SUB_YAML.replace("    quota_requests_per_minute: 10\n", "");
        let r = ProviderRegistry::from_yaml_str(&ok).expect("the provider reports its own limits");
        assert_eq!(
            r.get("claude-code/sonnet")
                .expect("entry")
                .quota_requests_per_minute,
            None
        );
    }

    #[test]
    fn registry_subscription_with_explicit_price_is_rejected() {
        let bad = SUB_YAML.replace(
            "    billing: subscription\n",
            "    billing: subscription\n    price: { version: v1, input_per_mtok_micros: 1, output_per_mtok_micros: 1 }\n",
        );
        let err = ProviderRegistry::from_yaml_str(&bad).expect_err("price forbidden");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn registry_subscription_on_anthropic_api_is_rejected() {
        let bad = SUB_YAML.replace("provider: claude_code", "provider: anthropic");
        let err = ProviderRegistry::from_yaml_str(&bad).expect_err("api key is metered");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn registry_metered_zero_price_is_rejected() {
        let bad = YAML.replace(
            "input_per_mtok_micros: 2000000, output_per_mtok_micros: 10000000",
            "input_per_mtok_micros: 0, output_per_mtok_micros: 0",
        );
        let err = ProviderRegistry::from_yaml_str(&bad).expect_err("zero price is a typo");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn registry_metered_entry_defaults_upstream_id_to_id() {
        let r = ProviderRegistry::from_yaml_str(YAML).expect("parse");
        let e = r.get("m-priced").expect("entry");
        assert_eq!(e.billing, Billing::Metered);
        assert_eq!(e.upstream_id, "m-priced");
    }

    #[test]
    fn registry_rejects_local_endpoint_in_config() {
        let bad = YAML.replace("https://ollama.com", "https://127.0.0.1:11434");
        let err = ProviderRegistry::from_yaml_str(&bad).expect_err("must reject");
        assert_eq!(err.code, ErrorCode::ProviderDisallowed);
    }

    #[test]
    fn registry_shipped_config_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/models.yaml");
        let r = ProviderRegistry::load(&path).expect("shipped models.yaml valid");
        assert!(r.iter().count() >= 2);
    }
}
