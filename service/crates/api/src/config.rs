//! Environment configuration; fails fast on missing or weak secrets.

use std::path::PathBuf;

use pair_core::error::{ErrorCode, PairError};

pub const MIN_TOKEN_LEN: usize = 16;
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
pub const DEFAULT_MODELS_CONFIG: &str = "config/models.yaml";
pub const DEFAULT_DATABASE_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub database_url: String,
    pub service_token: String,
    pub migrations_dir: PathBuf,
    pub data_dir: PathBuf,
    pub models_config: PathBuf,
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
        Ok(Self {
            bind: get("PAIR_BIND").unwrap_or_else(|| DEFAULT_BIND.to_owned()),
            database_url: get("DATABASE_URL").unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned()),
            service_token: token,
            migrations_dir: get("PAIR_MIGRATIONS_DIR")
                .map_or_else(|| PathBuf::from("migrations"), PathBuf::from),
            data_dir: get("PAIR_DATA_DIR").map_or_else(|| PathBuf::from("."), PathBuf::from),
            models_config: get("PAIR_MODELS_CONFIG")
                .map_or_else(|| PathBuf::from(DEFAULT_MODELS_CONFIG), PathBuf::from),
        })
    }

    pub fn from_env() -> Result<Self, PairError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
