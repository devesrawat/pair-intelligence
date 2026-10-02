//! Section budgets loaded from `config/context.yaml`.
use pair_core::error::{ErrorCode, PairError, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Per-section token ceilings plus the total input target.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBudgets {
    pub system_policy: u64,
    pub task_contract: u64,
    pub project: u64,
    pub memories: u64,
    pub tool_results: u64,
    pub conversation: u64,
    pub tool_schemas: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    pub default_profile: String,
    pub profiles: BTreeMap<String, ContextBudgets>,
}

fn bad(msg: impl Into<String>) -> PairError {
    PairError::new(
        ErrorCode::InvalidInput,
        format!("context config: {}", msg.into()),
    )
}

impl ContextConfig {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = serde_yaml_ng::from_str(text).map_err(|e| bad(e.to_string()))?;
        if !config.profiles.contains_key(&config.default_profile) {
            return Err(bad(format!(
                "default_profile '{}' not defined",
                config.default_profile
            )));
        }
        Ok(config)
    }

    pub fn profile(&self, name: &str) -> Result<ContextBudgets> {
        self.profiles
            .get(name)
            .cloned()
            .ok_or_else(|| bad(format!("unknown profile '{name}'")))
    }

    pub fn default_budgets(&self) -> Result<ContextBudgets> {
        self.profile(&self.default_profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_CONFIG: &str = include_str!("../../../../config/context.yaml");

    #[test]
    fn test_parse_repo_config_coding_profile_matches_spec() {
        let b = ContextConfig::parse(REPO_CONFIG).and_then(|c| c.profile("coding"));
        assert_eq!(
            b.map(|b| (
                b.system_policy,
                b.task_contract,
                b.project,
                b.memories,
                b.tool_results,
                b.conversation,
                b.tool_schemas,
                b.total
            ))
            .ok(),
            Some((800, 700, 1200, 1200, 5000, 1500, 800, 11200))
        );
    }

    #[test]
    fn test_parse_yaml_comments_are_accepted() {
        let text = "# top\ndefault_profile: a # inline\nprofiles:\n  # profile a\n  a:\n    system_policy: 1\n    task_contract: 1\n    project: 1\n    memories: 1\n    tool_results: 1\n    conversation: 1\n    tool_schemas: 1\n    total: 7 # sum\n";
        assert!(ContextConfig::parse(text).is_ok());
    }

    #[test]
    fn test_parse_malformed_yaml_fails_closed() {
        for bad in [
            "default_profile: [a\nprofiles: {",
            "default_profile: a\nprofiles:\n  a:\n    total: seven\n",
            "default_profile: b\nprofiles: {}\n",
            "",
        ] {
            assert!(ContextConfig::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_parse_missing_field_errors() {
        let r = ContextConfig::parse("default_profile: a\nprofiles:\n  a:\n    total: 1\n");
        assert!(r.is_err());
    }
}
