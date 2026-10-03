//! pair-api binary: loads config, runs migrations (if any), serves HTTP.

use std::time::Duration;

use pair_api::config::{self, Config};
use pair_api::healthcheck;
use pair_api::shutdown::shutdown_listener;
use pair_api::state::AppState;
use pair_models::provider::ProviderRegistry;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const POOL_MAX_CONNECTIONS: u32 = 10;
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--healthcheck") {
        let bind = std::env::var("PAIR_BIND").unwrap_or_else(|_| config::DEFAULT_BIND.to_owned());
        let healthy = healthcheck::probe(&healthcheck::local_addr(&bind)).await;
        std::process::exit(if healthy { 0 } else { 1 });
    }
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env()?;
    // An explicitly configured path that does not exist is a deployment error (typo, missing
    // mount): refuse to start rather than report ready with no providers or migrations.
    cfg.validate_paths()?;
    let pool = PgPoolOptions::new()
        .max_connections(POOL_MAX_CONNECTIONS)
        .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
        .connect(cfg.database_url.expose())
        .await?;

    if cfg.migrations_dir.is_dir() {
        let mut migrator = Migrator::new(cfg.migrations_dir.as_path()).await?;
        // Rollback to an older build must boot against a newer (additive) schema.
        migrator.set_ignore_missing(true);
        migrator.run(&pool).await?;
        tracing::info!(dir = %cfg.migrations_dir.display(), "migrations applied");
    } else {
        tracing::warn!(dir = %cfg.migrations_dir.display(), "migrations directory not found; /readyz will report not ready");
    }

    let mut state = AppState::new(pool, cfg.service_token.expose())
        .with_migrations_dir(cfg.migrations_dir)
        .with_migrations_required(true)
        .with_data_dir(cfg.data_dir);
    // Fails fast on an invalid registry; logs a warning per unverified model id.
    if cfg.models_config.is_file() {
        let registry = ProviderRegistry::load(&cfg.models_config)?;
        tracing::info!(path = %cfg.models_config.display(), "provider registry loaded");
        state = state.with_providers(Arc::new(registry));
    } else {
        tracing::warn!(path = %cfg.models_config.display(), "provider registry not found; /readyz will warn");
    }
    let shutdown = shutdown_listener()?;
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "pair-api listening");
    axum::serve(listener, pair_api::router(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    tracing::info!("pair-api stopped");
    Ok(())
}
