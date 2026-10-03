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
    /// Allowed URL schemes (`https` unless stated). A destination with no scheme is `https`.
    #[serde(default = "default_schemes")]
    pub schemes: Vec<String>,
    /// Allowed ports. Empty means only the default port of the scheme in use.
    #[serde(default)]
    pub ports: Vec<u16>,
}

fn default_schemes() -> Vec<String> {
    vec!["https".to_owned()]
}

pub const KNOWN_SCHEMES: [&str; 3] = ["http", "https", "ssh"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    pub version: String,
    pub tools: BTreeMap<String, ActionClass>,
    pub executables_allow: Vec<String>,
    /// Subset of `executables_allow` that can run arbitrary code (interpreters, build tools).
    /// Denied unless the engine runs inside the sandbox.
    #[serde(default)]
    pub code_exec: Vec<String>,
    /// File names (exact, or `*.suffix`) that are credentials wherever they appear.
    #[serde(default)]
    pub denied_names: Vec<String>,
    /// File names (exact, lowercase compare) that stay readable even when a `denied_names`
    /// pattern matches, e.g. `.env.example` under `.env.*`. Only exact names, never patterns.
    #[serde(default)]
    pub allowed_names: Vec<String>,
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
        if let Some(bad) = self
            .code_exec
            .iter()
            .find(|e| !self.executables_allow.contains(e))
        {
            return Err(PolicyError::Invalid(format!(
                "code_exec entry {bad:?} is not in executables_allow"
            )));
        }
        for rule in &self.egress {
            if rule.schemes.is_empty()
                || rule
                    .schemes
                    .iter()
                    .any(|s| !KNOWN_SCHEMES.contains(&s.as_str()))
            {
                return Err(PolicyError::Invalid(format!(
                    "invalid schemes for egress host {:?}",
                    rule.host
                )));
            }
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
