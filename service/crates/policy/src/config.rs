use crate::error::PolicyError;
use pair_core::types::DataClass;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Risk class of a registered tool. Assigned only by this config, never by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    Read,
    LocalEdit,
    LocalCommit,
    ExternalWrite,
    /// Merge, deployment, destructive operation, financial action.
    HighRisk,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressRule {
    /// Exact lowercase host, or `*.example.com` for subdomains only.
    pub host: String,
    pub data_classes: Vec<DataClass>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    pub version: String,
    pub tools: BTreeMap<String, ActionClass>,
    pub executables_allow: Vec<String>,
    pub denied_paths: Vec<String>,
    pub egress: Vec<EgressRule>,
}

impl PolicyConfig {
    /// Parses `config/policy.yaml`. Any parse or validation error means no policy exists.
    pub fn parse(text: &str) -> Result<Self, PolicyError> {
        let cfg: Self =
            serde_yaml_ng::from_str(text).map_err(|e| PolicyError::Invalid(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), PolicyError> {
        if self.version.trim().is_empty() {
            return Err(PolicyError::Invalid("version must not be empty".into()));
        }
        if self.tools.keys().any(|t| t.trim().is_empty()) {
            return Err(PolicyError::Invalid("tool names must not be empty".into()));
        }
        if self.denied_paths.is_empty() {
            return Err(PolicyError::Invalid(
                "denied_paths must not be empty".into(),
            ));
        }
        for rule in &self.egress {
            let host = rule.host.strip_prefix("*.").unwrap_or(&rule.host);
            if host.is_empty() || host != host.to_ascii_lowercase() || host.contains('*') {
                return Err(PolicyError::Invalid(format!(
                    "invalid egress host {:?}",
                    rule.host
                )));
            }
        }
        Ok(())
    }
}
