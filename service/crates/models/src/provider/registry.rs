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
}

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
    let price = match e.price {
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
