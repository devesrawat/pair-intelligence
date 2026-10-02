use crate::config::{ActionClass, PolicyConfig};
use crate::egress::check_destination;
use crate::error::PolicyError;
use crate::paths::PathGuard;
use crate::payload::payload_hash;
use pair_core::traits::Policy;
use pair_core::types::{ActionRequest, Decision, PolicyContext, PolicyOutcome};
use std::path::Path;
use std::sync::Arc;

const URL_SEPARATOR: &str = "://";

/// Config-driven policy. Tool risk class comes only from the config registry; nothing in the
/// request can declare itself safe.
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    config: Arc<PolicyConfig>,
    guard: PathGuard,
}

impl PolicyEngine {
    pub fn from_config_str(text: &str, home: &Path) -> Result<Self, PolicyError> {
        let config = PolicyConfig::parse(text)?;
        let guard = PathGuard::new(home, &config.denied_paths);
        Ok(Self {
            config: Arc::new(config),
            guard,
        })
    }

    pub fn from_config_file(path: &Path, home: &Path) -> Result<Self, PolicyError> {
        Self::from_config_str(&std::fs::read_to_string(path)?, home)
    }

    pub fn version(&self) -> &str {
        &self.config.version
    }

    fn evaluate(&self, req: &ActionRequest, ctx: &PolicyContext) -> Result<Decision, String> {
        if ctx.policy_version != self.config.version {
            return Err(format!(
                "policy version {:?} is not the active version",
                ctx.policy_version
            ));
        }
        let class = *self
            .config
            .tools
            .get(&req.tool)
            .ok_or_else(|| format!("tool {:?} is not registered", req.tool))?;
        let workspace = Path::new(&ctx.workspace_root)
            .canonicalize()
            .map_err(|e| format!("workspace root unusable: {e}"))?;
        self.check_executable(req)?;
        self.check_paths(req, &workspace)?;
        self.check_destinations(req)?;
        match class {
            ActionClass::Read | ActionClass::LocalEdit | ActionClass::LocalCommit => {
                Ok(Decision::Allow)
            }
            ActionClass::ExternalWrite | ActionClass::HighRisk => {
                let payload_hash = payload_hash(req, class)
                    .map_err(|e| format!("cannot canonicalize request: {e}"))?;
                Ok(Decision::NeedsApproval { payload_hash })
            }
        }
    }

    fn check_executable(&self, req: &ActionRequest) -> Result<(), String> {
        match req.executable.as_deref() {
            Some(exe) if !self.config.executables_allow.iter().any(|a| a == exe) => {
                Err(format!("executable {exe:?} is not on the allowlist"))
            }
            _ => Ok(()),
        }
    }

    fn check_paths(&self, req: &ActionRequest, workspace: &Path) -> Result<(), String> {
        let arg_paths = req
            .args
            .iter()
            .flat_map(|a| arg_values(a))
            .filter_map(path_like);
        req.paths
            .iter()
            .map(String::as_str)
            .chain(arg_paths)
            .try_for_each(|p| self.guard.check(workspace, p).map(drop))
    }

    fn check_destinations(&self, req: &ActionRequest) -> Result<(), String> {
        let arg_urls = req
            .args
            .iter()
            .flat_map(|a| arg_values(a))
            .filter_map(url_like);
        req.destination
            .as_deref()
            .into_iter()
            .chain(arg_urls)
            .try_for_each(|d| check_destination(&self.config.egress, d, req.data_class))
    }
}

impl Policy for PolicyEngine {
    fn authorize(&self, req: &ActionRequest, ctx: &PolicyContext) -> PolicyOutcome {
        let decision = self.evaluate(req, ctx).unwrap_or_else(|reason| {
            tracing::warn!(tool = %req.tool, task = %req.task, trace = %req.trace, %reason, "policy denied action");
            Decision::Deny { reason }
        });
        tracing::info!(tool = %req.tool, task = %req.task, trace = %req.trace, ?decision, "policy decision");
        PolicyOutcome {
            decision,
            policy_version: self.config.version.clone(),
        }
    }
}

/// The value part of an argument (`--flag=value` or `KEY=value` gives `value`).
fn assigned_value(arg: &str) -> &str {
    match arg.split_once('=') {
        Some((name, value)) if !name.contains('/') => value,
        _ => arg,
    }
}

/// Every value an argument could carry: the argument itself, its `=` value, and for a short
/// flag with an attached value (`-o/etc/x`) the text after the flag letter.
fn arg_values(arg: &str) -> impl Iterator<Item = &str> {
    let attached = arg
        .strip_prefix('-')
        .filter(|rest| !rest.starts_with('-'))
        .and_then(|rest| rest.chars().next().map(|flag| &rest[flag.len_utf8()..]))
        .filter(|value| !value.is_empty());
    [Some(arg), attached]
        .into_iter()
        .flatten()
        .map(assigned_value)
}

/// Values that could name a filesystem location are treated as paths (fail closed).
fn path_like(value: &str) -> Option<&str> {
    let is_path = !value.contains(URL_SEPARATOR)
        && (value.contains('/') || value.starts_with('~') || value == "..");
    is_path.then_some(value)
}

fn url_like(value: &str) -> Option<&str> {
    value.contains(URL_SEPARATOR).then_some(value)
}
