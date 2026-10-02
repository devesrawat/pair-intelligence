//! Typed view of the `routing:` section of `config/routing.yaml`.
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::types::DataClass;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Classifier operating mode. Only `Active` may change the selected generation tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierMode {
    Disabled,
    Shadow,
    Active,
}

impl ClassifierMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Shadow => "shadow",
            Self::Active => "active",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClassifierConfig {
    pub mode: ClassifierMode,
    pub endpoint: String,
    pub model: String,
    pub deadline_ms: u64,
    pub input_price_micros_per_mtok: i64,
    pub price_version: String,
    pub questions_path: String,
    pub allowed_data_classes: Vec<DataClass>,
    #[serde(default)]
    pub active_categories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BaselineConfig {
    pub default_tier: String,
    pub by_intent: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThresholdEntry {
    pub model_version: String,
    pub question_version: String,
    pub intent: f64,
    pub difficulty: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub provider: String,
    pub tier: String,
    pub capabilities: Vec<String>,
    pub context_tokens: u64,
    #[serde(default = "default_true")]
    pub available: bool,
    pub input_price_micros_per_mtok: i64,
    pub output_price_micros_per_mtok: i64,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingConfig {
    pub classifier: ClassifierConfig,
    pub max_model_attempts: usize,
    pub task_cap_micros: i64,
    pub max_output_tokens: u64,
    pub tier_order: Vec<String>,
    pub tier_by_difficulty: BTreeMap<String, String>,
    pub baseline: BaselineConfig,
    pub thresholds: Vec<ThresholdEntry>,
    pub data_class_providers: HashMap<DataClass, Vec<String>>,
    pub candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
struct RoutingFile {
    routing: RoutingConfig,
}

impl RoutingConfig {
    pub fn from_yaml(text: &str) -> Result<Self> {
        let file: RoutingFile = serde_yaml_ng::from_str(text)
            .map_err(|e| PairError::new(ErrorCode::InvalidInput, format!("routing config: {e}")))?;
        file.routing.validate()?;
        Ok(file.routing)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            PairError::new(
                ErrorCode::InvalidInput,
                format!("read {}: {e}", path.display()),
            )
        })?;
        Self::from_yaml(&text)
    }

    fn validate(&self) -> Result<()> {
        let known = |tier: &str| self.tier_order.iter().any(|t| t == tier);
        let tiers_ok = known(&self.baseline.default_tier)
            && self.baseline.by_intent.values().all(|t| known(t))
            && self.tier_by_difficulty.values().all(|t| known(t))
            && self.candidates.iter().all(|c| known(&c.tier));
        if !tiers_ok || self.max_model_attempts == 0 {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "routing config references an unknown tier or zero attempts",
            ));
        }
        Ok(())
    }

    /// Thresholds for the exact (returned model version, question version) pair, if calibrated.
    pub fn thresholds_for(
        &self,
        model_version: &str,
        question_version: &str,
    ) -> Option<&ThresholdEntry> {
        self.thresholds
            .iter()
            .find(|t| t.model_version == model_version && t.question_version == question_version)
    }

    pub fn baseline_tier(&self, intent: &str) -> &str {
        self.baseline
            .by_intent
            .get(intent)
            .map_or(self.baseline.default_tier.as_str(), String::as_str)
    }
}

#[cfg(test)]
pub(crate) fn test_config() -> RoutingConfig {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/routing.yaml");
    RoutingConfig::from_path(&path).expect("config/routing.yaml loads")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_path_shipped_config_defaults_to_shadow_without_active_categories() {
        let cfg = test_config();
        assert_eq!(cfg.classifier.mode, ClassifierMode::Shadow);
        assert!(cfg.classifier.active_categories.is_empty());
        assert_eq!(cfg.max_model_attempts, 3);
        assert_eq!(cfg.classifier.deadline_ms, 1000);
    }

    #[test]
    fn test_from_yaml_unknown_tier_rejected() {
        let bad = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/routing.yaml"),
        )
        .expect("read")
        .replace("default_tier: strong", "default_tier: bogus");
        assert!(RoutingConfig::from_yaml(&bad).is_err());
    }
}
