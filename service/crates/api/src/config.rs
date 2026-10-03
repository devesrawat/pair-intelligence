//! Environment configuration; fails fast on missing or weak secrets and on explicitly
//! configured paths that do not exist.

use std::path::PathBuf;
use std::time::Duration;

use pair_core::error::{ErrorCode, PairError};
use pair_telemetry::Secret;

pub const MIN_TOKEN_LEN: usize = 16;
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
pub const DEFAULT_MODELS_CONFIG: &str = "config/models.yaml";
pub const DEFAULT_DATABASE_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";
pub const DEFAULT_BUDGET_CONFIG: &str = "config/budget.yaml";
pub const DEFAULT_POLICY_CONFIG: &str = "config/policy.yaml";
pub const DEFAULT_CONTEXT_CONFIG: &str = "config/context.yaml";
const ENABLED_FLAG: &str = "1";

/// Seconds-valued settings: (name, default, minimum). Every value is validated, never clamped.
const SECS_SETTINGS: [(&str, u64, u64); 5] = [
    ("PAIR_SWEEP_INTERVAL_SECS", 15, 1),
    ("PAIR_ORPHAN_INTERVAL_SECS", 60, 1),
    ("PAIR_WORKER_INTERVAL_SECS", 2, 1),
    ("PAIR_SHUTDOWN_DRAIN_SECS", 20, 1),
    ("PAIR_TURN_BUDGET_SECS", 25, 1),
];
/// Must stay below the 30 s HTTP request timeout, or a turn can be cut off mid-flight.
const MAX_TURN_BUDGET_SECS: u64 = 28;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    /// Wrapped so `Debug` can never print credentials.
    pub database_url: Secret,
    pub service_token: Secret,
    pub migrations_dir: PathBuf,
    pub data_dir: PathBuf,
    pub models_config: PathBuf,
    /// `PAIR_MIGRATIONS_DIR` was set explicitly: a missing directory is a startup error.
    pub migrations_dir_explicit: bool,
    /// `PAIR_MODELS_CONFIG` was set explicitly: a missing file is a startup error.
    pub models_config_explicit: bool,
    pub budget_config: PathBuf,
    pub budget_config_explicit: bool,
    pub policy_config: PathBuf,
    pub policy_config_explicit: bool,
    pub context_config: PathBuf,
    pub context_config_explicit: bool,
    /// Workspace the policy engine resolves paths against; unset makes `/v1/policy/authorize` deny.
    pub workspace_root: Option<PathBuf>,
    /// Second credential for `POST /v1/approvals`; unset disables approval creation.
    pub approver_token: Option<Secret>,
    pub anthropic_api_key: Option<Secret>,
    pub ollama_api_key: Option<Secret>,
    pub typesafe_api_key: Option<Secret>,
    pub allow_unverified_model_ids: bool,
    /// Home directory the policy engine expands `~` against (credential-path denial).
    pub home: Option<PathBuf>,
    pub sweep_interval: Duration,
    pub orphan_interval: Duration,
    pub worker_interval: Duration,
    pub shutdown_drain: Duration,
    pub turn_budget: Duration,
}

/// Empty counts as unset, so compose can pass `${VAR:-}` through.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

impl Config {
    /// Build from an arbitrary lookup (injectable for tests).
    pub fn from_lookup<F>(get: F) -> Result<Self, PairError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let token = get("PAIR_SERVICE_TOKEN").ok_or_else(|| {
            PairError::new(ErrorCode::InvalidInput, "PAIR_SERVICE_TOKEN is required")
        })?;
        if token.len() < MIN_TOKEN_LEN {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("PAIR_SERVICE_TOKEN must be at least {MIN_TOKEN_LEN} characters"),
            ));
        }
        let migrations_dir = get("PAIR_MIGRATIONS_DIR");
        let models_config = get("PAIR_MODELS_CONFIG");
        let budget_config = get("PAIR_BUDGET_CONFIG");
        let policy_config = get("PAIR_POLICY_CONFIG");
        let context_config = get("PAIR_CONTEXT_CONFIG");
        let approver_token = non_empty(get("PAIR_APPROVER_TOKEN"));
        if approver_token.as_deref().is_some_and(|t| t.len() < MIN_TOKEN_LEN) {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("PAIR_APPROVER_TOKEN must be at least {MIN_TOKEN_LEN} characters"),
            ));
        }
        if approver_token.as_deref() == Some(token.as_str()) {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "PAIR_APPROVER_TOKEN must differ from PAIR_SERVICE_TOKEN",
            ));
        }
        let secs = |name: &str, default: u64, min: u64| -> Result<Duration, PairError> {
            let value = match non_empty(get(name)) {
                None => default,
                Some(raw) => raw.trim().parse::<u64>().map_err(|_| {
                    PairError::new(ErrorCode::InvalidInput, format!("{name} must be an integer"))
                })?,
            };
            if value < min {
                return Err(PairError::new(
                    ErrorCode::InvalidInput,
                    format!("{name} must be at least {min}"),
                ));
            }
            Ok(Duration::from_secs(value))
        };
        let [sweep, orphan, worker, drain, turn] = SECS_SETTINGS.map(|(n, d, m)| secs(n, d, m));
        let turn_budget = turn?;
        if turn_budget.as_secs() > MAX_TURN_BUDGET_SECS {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("PAIR_TURN_BUDGET_SECS must be at most {MAX_TURN_BUDGET_SECS}"),
            ));
        }
        Ok(Self {
            bind: get("PAIR_BIND").unwrap_or_else(|| DEFAULT_BIND.to_owned()),
            database_url: Secret::new(
                get("DATABASE_URL").unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned()),
            ),
            service_token: Secret::new(token),
            migrations_dir_explicit: migrations_dir.is_some(),
            migrations_dir: migrations_dir
                .map_or_else(|| PathBuf::from("migrations"), PathBuf::from),
            data_dir: get("PAIR_DATA_DIR").map_or_else(|| PathBuf::from("."), PathBuf::from),
            models_config_explicit: models_config.is_some(),
            models_config: models_config
                .map_or_else(|| PathBuf::from(DEFAULT_MODELS_CONFIG), PathBuf::from),
            budget_config_explicit: budget_config.is_some(),
            budget_config: budget_config
                .map_or_else(|| PathBuf::from(DEFAULT_BUDGET_CONFIG), PathBuf::from),
            policy_config_explicit: policy_config.is_some(),
            policy_config: policy_config
                .map_or_else(|| PathBuf::from(DEFAULT_POLICY_CONFIG), PathBuf::from),
            context_config_explicit: context_config.is_some(),
            context_config: context_config
                .map_or_else(|| PathBuf::from(DEFAULT_CONTEXT_CONFIG), PathBuf::from),
            workspace_root: non_empty(get("PAIR_WORKSPACE_ROOT")).map(PathBuf::from),
            approver_token: approver_token.map(Secret::new),
            anthropic_api_key: non_empty(get("ANTHROPIC_API_KEY")).map(Secret::new),
            ollama_api_key: non_empty(get("OLLAMA_API_KEY")).map(Secret::new),
            typesafe_api_key: non_empty(get("TYPESAFE_API_KEY")).map(Secret::new),
            allow_unverified_model_ids: get("PAIR_ALLOW_UNVERIFIED_MODEL_IDS")
                .is_some_and(|v| v == ENABLED_FLAG),
            home: non_empty(get("HOME")).map(PathBuf::from),
            sweep_interval: sweep?,
            orphan_interval: orphan?,
            worker_interval: worker?,
            shutdown_drain: drain?,
            turn_budget,
        })
    }

    pub fn from_env() -> Result<Self, PairError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Fail fast when an explicitly configured path does not exist: a typo or a missing mount
    /// must not silently degrade to "no providers" or "no migrations".
    pub fn validate_paths(&self) -> Result<(), PairError> {
        if self.models_config_explicit && !self.models_config.is_file() {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!(
                    "PAIR_MODELS_CONFIG is set but {} is not a file",
                    self.models_config.display()
                ),
            ));
        }
        for (name, explicit, path) in [
            ("PAIR_BUDGET_CONFIG", self.budget_config_explicit, &self.budget_config),
            ("PAIR_POLICY_CONFIG", self.policy_config_explicit, &self.policy_config),
            ("PAIR_CONTEXT_CONFIG", self.context_config_explicit, &self.context_config),
        ] {
            if explicit && !path.is_file() {
                return Err(PairError::new(
                    ErrorCode::InvalidInput,
                    format!("{name} is set but {} is not a file", path.display()),
                ));
            }
        }
        if self.migrations_dir_explicit && !self.migrations_dir.is_dir() {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!(
                    "PAIR_MIGRATIONS_DIR is set but {} is not a directory",
                    self.migrations_dir.display()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_TOKEN: &str = "tok-0123456789abcdef";

    fn lookup(extra: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            if k == "PAIR_SERVICE_TOKEN" {
                return Some(GOOD_TOKEN.to_owned());
            }
            extra
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn test_from_lookup_missing_token_is_error() {
        assert!(Config::from_lookup(|_| None).is_err());
    }

    #[test]
    fn test_from_lookup_short_token_is_error() {
        let r = Config::from_lookup(|k| (k == "PAIR_SERVICE_TOKEN").then(|| "short".to_owned()));
        assert!(r.is_err());
    }

    #[test]
    fn test_from_lookup_defaults_applied() {
        let r =
            Config::from_lookup(|k| (k == "PAIR_SERVICE_TOKEN").then(|| "x".repeat(MIN_TOKEN_LEN)))
                .expect("valid config");
        assert_eq!(r.bind, DEFAULT_BIND);
    }

    #[test]
    fn config_debug_never_prints_secrets() {
        let cfg = Config::from_lookup(lookup(&[(
            "DATABASE_URL",
            "postgres://pair:hunter2-db-pass@127.0.0.1:5432/pair",
        )]))
        .expect("valid config");
        let shown = format!("{cfg:?}");
        assert!(!shown.contains(GOOD_TOKEN), "{shown}");
        assert!(!shown.contains("hunter2-db-pass"), "{shown}");
        assert!(shown.contains("REDACTED"), "{shown}");
    }

    #[test]
    fn startup_fails_when_configured_models_config_missing() {
        let cfg = Config::from_lookup(lookup(&[(
            "PAIR_MODELS_CONFIG",
            "/nonexistent/models.yaml",
        )]))
        .expect("valid config");
        assert!(cfg.validate_paths().is_err());
    }

    #[test]
    fn startup_fails_when_configured_migrations_dir_missing() {
        let cfg = Config::from_lookup(lookup(&[(
            "PAIR_MIGRATIONS_DIR",
            "/nonexistent/migrations",
        )]))
        .expect("valid config");
        assert!(cfg.validate_paths().is_err());
    }

    fn with(name: &str, value: &str) -> impl Fn(&str) -> Option<String> {
        let (name, value) = (name.to_owned(), value.to_owned());
        move |k| {
            if k == "PAIR_SERVICE_TOKEN" {
                Some(GOOD_TOKEN.to_owned())
            } else if k == name {
                Some(value.clone())
            } else {
                None
            }
        }
    }

    #[test]
    fn approver_token_must_be_long_and_distinct_from_the_service_token() {
        assert!(Config::from_lookup(with("PAIR_APPROVER_TOKEN", "short")).is_err());
        assert!(Config::from_lookup(with("PAIR_APPROVER_TOKEN", GOOD_TOKEN)).is_err());
        let fine = Config::from_lookup(with("PAIR_APPROVER_TOKEN", "approver-0123456789abc"))
            .expect("valid");
        assert!(fine.approver_token.is_some());
        let unset = Config::from_lookup(with("PAIR_APPROVER_TOKEN", "")).expect("valid");
        assert!(unset.approver_token.is_none(), "empty means unset");
    }

    #[test]
    fn debug_never_prints_provider_or_approver_secrets() {
        let cfg = Config::from_lookup(|k| match k {
            "PAIR_SERVICE_TOKEN" => Some(GOOD_TOKEN.to_owned()),
            "ANTHROPIC_API_KEY" => Some("sk-ant-secret-value".to_owned()),
            "TYPESAFE_API_KEY" => Some("ts-secret-value".to_owned()),
            "PAIR_APPROVER_TOKEN" => Some("approver-0123456789abc".to_owned()),
            _ => None,
        })
        .expect("valid");
        let shown = format!("{cfg:?}");
        for secret in ["sk-ant-secret-value", "ts-secret-value", "approver-0123456789abc"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn durations_default_parse_and_reject_bad_values() {
        let d = Config::from_lookup(lookup(&[])).expect("valid");
        assert_eq!(d.sweep_interval, Duration::from_secs(15));
        assert_eq!(d.turn_budget, Duration::from_secs(25));
        let custom = Config::from_lookup(with("PAIR_SWEEP_INTERVAL_SECS", "5")).expect("valid");
        assert_eq!(custom.sweep_interval, Duration::from_secs(5));
        for (name, bad) in [
            ("PAIR_SWEEP_INTERVAL_SECS", "0"),
            ("PAIR_SWEEP_INTERVAL_SECS", "soon"),
            ("PAIR_TURN_BUDGET_SECS", "300"),
        ] {
            assert!(Config::from_lookup(with(name, bad)).is_err(), "{name}={bad}");
        }
    }

    #[test]
    fn startup_fails_when_configured_budget_policy_or_context_config_missing() {
        for name in ["PAIR_BUDGET_CONFIG", "PAIR_POLICY_CONFIG", "PAIR_CONTEXT_CONFIG"] {
            let cfg = Config::from_lookup(with(name, "/nonexistent/file.yaml")).expect("valid");
            assert!(cfg.validate_paths().is_err(), "{name}");
        }
    }

    #[test]
    fn every_env_var_the_config_reads_is_documented_in_api_yaml() {
        let seen = std::cell::RefCell::new(Vec::new());
        let _ = Config::from_lookup(|k| {
            seen.borrow_mut().push(k.to_owned());
            (k == "PAIR_SERVICE_TOKEN").then(|| GOOD_TOKEN.to_owned())
        });
        let doc = include_str!("../../../../config/api.yaml");
        let seen = seen.into_inner();
        assert!(seen.len() > 15, "the lookup recorder saw {}", seen.len());
        for key in seen.iter().map(String::as_str).chain(["RUST_LOG"]) {
            assert!(doc.contains(key), "{key} is not documented in config/api.yaml");
        }
    }

    #[test]
    fn startup_tolerates_missing_default_paths() {
        let cfg = Config::from_lookup(lookup(&[])).expect("valid config");
        assert!(!cfg.models_config_explicit && !cfg.migrations_dir_explicit);
        assert!(cfg.validate_paths().is_ok());
    }
}
