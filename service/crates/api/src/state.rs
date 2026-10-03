//! Shared application state.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pair_core::error::{ErrorCode, PairError};
use pair_telemetry::health::{statfs_via_df, DiskStats};
use sqlx::PgPool;

use crate::background::Liveness;
use crate::limits::Limits;
use crate::providers::{ProviderHealth, StubProviderHealth};
use crate::services::Services;

pub type DiskProbe = Arc<dyn Fn(&Path) -> io::Result<DiskStats> + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    token: Arc<str>,
    pub(crate) migrations_dir: Arc<PathBuf>,
    pub(crate) data_dir: Arc<PathBuf>,
    pub(crate) providers: Arc<dyn ProviderHealth>,
    pub(crate) disk_probe: DiskProbe,
    pub(crate) migrations_required: bool,
    pub(crate) limits: Limits,
    pub(crate) services: Option<Arc<Services>>,
    pub(crate) services_required: bool,
    pub(crate) liveness: Option<Arc<Liveness>>,
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
            migrations_required: false,
            limits: Limits::default(),
            services: None,
            services_required: false,
            liveness: None,
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

    /// When set, `/readyz` fails if no migrations are found on disk or none are applied
    /// (a missing mount must not read as "nothing to migrate").
    pub fn with_migrations_required(mut self, required: bool) -> Self {
        self.migrations_required = required;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_disk_probe(mut self, probe: DiskProbe) -> Self {
        self.disk_probe = probe;
        self
    }

    /// Host the budget, policy, routing and conversation crates behind the `/v1` endpoints.
    pub fn with_services(mut self, services: Services) -> Self {
        self.services = Some(Arc::new(services));
        self
    }

    /// When set, `/readyz` fails while the services are not configured (the service binary sets
    /// it: an instance that cannot enforce a budget or policy must not look ready).
    pub fn with_services_required(mut self, required: bool) -> Self {
        self.services_required = required;
        self
    }

    /// Liveness of the background tasks, reported by `/readyz`.
    pub fn with_liveness(mut self, liveness: Arc<Liveness>) -> Self {
        self.liveness = Some(liveness);
        self
    }

    /// The hosted services, or a 503 when the instance started without them.
    pub fn services(&self) -> Result<Arc<Services>, PairError> {
        self.services.clone().ok_or_else(|| {
            PairError::new(
                ErrorCode::ProviderUnavailable,
                "budget, policy and routing services are not configured on this instance",
            )
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
}
