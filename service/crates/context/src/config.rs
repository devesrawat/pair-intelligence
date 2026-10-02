//! Section budgets loaded from `config/context.yaml`. The file uses a tiny YAML subset
//! (two-level maps of integers), parsed here to avoid a YAML dependency.
use pair_core::error::{ErrorCode, PairError, Result};
use std::collections::BTreeMap;

/// Per-section token ceilings plus the total input target.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct ContextConfig {
    pub default_profile: String,
    pub profiles: BTreeMap<String, ContextBudgets>,
}

const PROFILE_INDENT: usize = 2;
const FIELD_INDENT: usize = 4;

fn bad(msg: impl Into<String>) -> PairError {
    PairError::new(
        ErrorCode::InvalidInput,
        format!("context config: {}", msg.into()),
    )
}

impl ContextConfig {
    pub fn parse(text: &str) -> Result<Self> {
        let mut default_profile: Option<String> = None;
        let mut raw: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for (n, line) in text.lines().enumerate() {
            let body = line.split('#').next().unwrap_or("").trim_end();
            if body.trim().is_empty() {
                continue;
            }
            let indent = body.len() - body.trim_start().len();
            let (key, val) = body
                .trim()
                .split_once(':')
                .ok_or_else(|| bad(format!("line {}: expected key: value", n + 1)))?;
            let (key, val) = (key.trim(), val.trim());
            match indent {
                0 if key == "default_profile" => default_profile = Some(val.to_string()),
                0 if key == "profiles" && val.is_empty() => {}
                PROFILE_INDENT if val.is_empty() => {
                    raw.entry(key.to_string()).or_default();
                    current = Some(key.to_string());
                }
                FIELD_INDENT => {
                    let profile = current
                        .as_ref()
                        .ok_or_else(|| bad(format!("line {}: field outside profile", n + 1)))?;
                    let v: u64 = val
                        .parse()
                        .map_err(|_| bad(format!("line {}: '{val}' is not an integer", n + 1)))?;
                    raw.entry(profile.clone())
                        .or_default()
                        .insert(key.to_string(), v);
                }
                _ => return Err(bad(format!("line {}: unexpected structure", n + 1))),
            }
        }
        let default_profile = default_profile.ok_or_else(|| bad("missing default_profile"))?;
        let profiles = raw
            .into_iter()
            .map(|(name, fields)| Ok((name.clone(), budgets_from(&name, &fields)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        if !profiles.contains_key(&default_profile) {
            return Err(bad(format!(
                "default_profile '{default_profile}' not defined"
            )));
        }
        Ok(Self {
            default_profile,
            profiles,
        })
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

fn budgets_from(name: &str, f: &BTreeMap<String, u64>) -> Result<ContextBudgets> {
    let get = |k: &str| {
        f.get(k)
            .copied()
            .ok_or_else(|| bad(format!("profile '{name}' missing '{k}'")))
    };
    Ok(ContextBudgets {
        system_policy: get("system_policy")?,
        task_contract: get("task_contract")?,
        project: get("project")?,
        memories: get("memories")?,
        tool_results: get("tool_results")?,
        conversation: get("conversation")?,
        tool_schemas: get("tool_schemas")?,
        total: get("total")?,
    })
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
    fn test_parse_missing_field_errors() {
        let r = ContextConfig::parse("default_profile: a\nprofiles:\n  a:\n    total: 1\n");
        assert!(r.is_err());
    }
}
