use crate::args;
use crate::config::{ActionClass, PolicyConfig};
use crate::egress::check_destination;
use crate::error::PolicyError;
use crate::exec_rules::{analyze, ExecClass};
use crate::paths::PathGuard;
use crate::payload::payload_hash;
use pair_core::traits::Policy;
use pair_core::types::{ActionRequest, Decision, PolicyContext, PolicyOutcome};
use std::path::Path;
use std::sync::Arc;

/// Config-driven policy. Tool risk class comes only from the config registry; nothing in the
/// request can declare itself safe.
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    config: Arc<PolicyConfig>,
    guard: PathGuard,
    sandboxed: bool,
}

impl PolicyEngine {
    pub fn from_config_str(text: &str, home: &Path) -> Result<Self, PolicyError> {
        let config = PolicyConfig::parse(text)?;
        let guard = PathGuard::new(home, &config.denied_paths, &config.denied_names);
        Ok(Self {
            config: Arc::new(config),
            guard,
            sandboxed: false,
        })
    }

    pub fn from_config_file(path: &Path, home: &Path) -> Result<Self, PolicyError> {
        Self::from_config_str(&std::fs::read_to_string(path)?, home)
    }

    /// An engine for code that itself runs inside the sandbox (container with no network and
    /// only the worktree mounted). Only this engine permits `code_exec` executables; callers
    /// must not hand it to anything that executes on the host.
    pub fn with_sandbox(&self) -> Self {
        Self {
            sandboxed: true,
            ..self.clone()
        }
    }

    pub fn is_sandboxed(&self) -> bool {
        self.sandboxed
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
        let extra_paths = self.check_executable(req)?;
        self.check_paths(req, &workspace, &extra_paths)?;
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

    /// Returns the extra path operands the executable analysis found.
    fn check_executable(&self, req: &ActionRequest) -> Result<Vec<String>, String> {
        let Some(exe) = req.executable.as_deref() else {
            return Ok(Vec::new());
        };
        if !self.config.executables_allow.iter().any(|a| a == exe) {
            return Err(format!("executable {exe:?} is not on the allowlist"));
        }
        let analysis = analyze(exe, &req.args, &self.config.code_exec)?;
        if analysis.class == ExecClass::CodeExec && !self.sandboxed {
            return Err(format!(
                "{exe:?} can execute arbitrary code and is only permitted inside the sandbox"
            ));
        }
        Ok(analysis.extra_paths)
    }

    fn check_paths(
        &self,
        req: &ActionRequest,
        workspace: &Path,
        extra_paths: &[String],
    ) -> Result<(), String> {
        let refs = args::extract_for(req.executable.as_deref(), &req.args);
        req.paths
            .iter()
            .chain(extra_paths)
            .chain(&refs.paths)
            .try_for_each(|p| self.guard.check(workspace, p).map(drop))
    }

    fn check_destinations(&self, req: &ActionRequest) -> Result<(), String> {
        let refs = args::extract_for(req.executable.as_deref(), &req.args);
        req.destination
            .iter()
            .chain(&refs.destinations)
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
