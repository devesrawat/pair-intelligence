//! Per-repository authoritative commands, read from `<repo>/.pair/repo.json` of the
//! original checkout (never from the task worktree, which the task may edit).
use pair_core::error::{ErrorCode, PairError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const REPO_CONFIG_PATH: &str = ".pair/repo.json";
const DEFAULT_TIMEOUT_SECS: u64 = 600;

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Build / lint / type-check argv vectors, run first.
    #[serde(default)]
    pub build: Vec<Vec<String>>,
    /// Acceptance argv vectors; at least one is required.
    pub acceptance: Vec<Vec<String>>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Extra environment variable names forwarded to commands (e.g. CARGO_HOME).
    #[serde(default)]
    pub env_passthrough: Vec<String>,
}

impl RepoConfig {
    pub fn parse(json: &str) -> Result<Self> {
        let cfg: RepoConfig = serde_json::from_str(json).map_err(|e| {
            PairError::new(ErrorCode::InvalidInput, format!("invalid repo config: {e}"))
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(repo: &Path) -> Result<Self> {
        let path = repo.join(REPO_CONFIG_PATH);
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            PairError::new(
                ErrorCode::NotFound,
                format!("repo config {}: {e}", path.display()),
            )
        })?;
        Self::parse(&raw)
    }

    fn validate(&self) -> Result<()> {
        if self.acceptance.is_empty() {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "repo config needs at least one acceptance command",
            ));
        }
        if self
            .build
            .iter()
            .chain(&self.acceptance)
            .any(|c| c.is_empty() || c[0].trim().is_empty())
        {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "repo config contains an empty command",
            ));
        }
        if self.timeout_secs == 0 {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "timeout_secs must be positive",
            ));
        }
        if let Some(bad) = self
            .env_passthrough
            .iter()
            .find(|n| super::runner::looks_secret_name(n))
        {
            return Err(PairError::new(
                ErrorCode::PolicyDenied,
                format!("env passthrough of secret-like name {bad}"),
            ));
        }
        Ok(())
    }
}
