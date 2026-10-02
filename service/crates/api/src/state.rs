//! Shared application state.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pair_telemetry::health::{statfs_via_df, DiskStats};
use sqlx::PgPool;

use crate::providers::{ProviderHealth, StubProviderHealth};

pub type DiskProbe = Arc<dyn Fn(&Path) -> io::Result<DiskStats> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    token: Arc<str>,
    pub(crate) migrations_dir: Arc<PathBuf>,
    pub(crate) data_dir: Arc<PathBuf>,
    pub(crate) providers: Arc<dyn ProviderHealth>,
    pub(crate) disk_probe: DiskProbe,
}

impl AppState {
    pub fn new(pool: PgPool, token: impl Into<Arc<str>>) -> Self {
        Self {
            pool,
            token: token.into(),
            migrations_dir: Arc::new(PathBuf::from("migrations")),
            data_dir: Arc::new(PathBuf::from(".")),
            providers: Arc::new(StubProviderHealth),
            disk_probe: Arc::new(statfs_via_df),
        }
    }

    pub fn with_migrations_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.migrations_dir = Arc::new(dir.into());
        self
    }

    pub fn with_data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Arc::new(dir.into());
        self
    }

    pub fn with_providers(mut self, providers: Arc<dyn ProviderHealth>) -> Self {
        self.providers = providers;
        self
    }

    pub fn with_disk_probe(mut self, probe: DiskProbe) -> Self {
        self.disk_probe = probe;
        self
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}
