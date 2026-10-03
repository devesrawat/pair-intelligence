//! Environment configuration; fails fast on missing or weak secrets and on explicitly
//! configured paths that do not exist.

use std::path::PathBuf;

use pair_core::error::{ErrorCode, PairError};
use pair_telemetry::Secret;

pub const MIN_TOKEN_LEN: usize = 16;
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
pub const DEFAULT_MODELS_CONFIG: &str = "config/models.yaml";
pub const DEFAULT_DATABASE_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";

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

    #[test]
    fn startup_tolerates_missing_default_paths() {
        let cfg = Config::from_lookup(lookup(&[])).expect("valid config");
        assert!(!cfg.models_config_explicit && !cfg.migrations_dir_explicit);
        assert!(cfg.validate_paths().is_ok());
    }
}
