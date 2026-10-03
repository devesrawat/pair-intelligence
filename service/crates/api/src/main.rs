//! pair-api binary: loads config, runs migrations (if any), hosts the services and background
//! tasks, serves HTTP until SIGTERM/Ctrl-C, then drains the background tasks.

use std::sync::Arc;
use std::time::Duration;

use pair_api::background::tasks::{orphan_reconciler, sweeper, worker_loop};
use pair_api::background::{Background, Liveness};
use pair_api::bootstrap::load_services;
use pair_api::config::{self, Config};
use pair_api::healthcheck;
use pair_api::shutdown::shutdown_listener;
use pair_api::state::AppState;
use pair_jobs::{JobConfig, JobStore, StepHandler};
use pair_models::provider::ProviderRegistry;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
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

    let mut state = AppState::new(pool.clone(), cfg.service_token.expose())
        .with_migrations_dir(cfg.migrations_dir.clone())
        .with_migrations_required(true)
        .with_services_required(true)
        .with_data_dir(cfg.data_dir.clone());
    // Fails fast on an invalid registry; logs a warning per unverified model id.
    let registry = if cfg.models_config.is_file() {
        let registry = Arc::new(ProviderRegistry::load(&cfg.models_config)?);
        tracing::info!(path = %cfg.models_config.display(), "provider registry loaded");
        state = state.with_providers(registry.clone());
        Some(registry)
    } else {
        tracing::warn!(path = %cfg.models_config.display(), "provider registry not found; /readyz will warn");
        None
    };
    // A file that exists but is invalid is a startup error, like an invalid registry.
    match load_services(&cfg, pool.clone(), registry)? {
        Some(services) => {
            tracing::info!("budget, policy, routing and conversation services hosted");
            state = state.with_services(services);
        }
        None => {
            tracing::warn!("services not configured: /v1 endpoints answer 503, /readyz not ready")
        }
    }

    let liveness = Arc::new(Liveness::default());
    let jobs = JobStore::new(pool, JobConfig::default());
    // No workflow step handler is registered yet (the workflows need a repository and a sandbox),
    // so no worker loop runs; the sweeper and the orphan reconciler do.
    let handlers: Vec<(String, Arc<dyn StepHandler>)> = Vec::new();
    let mut tasks = vec![
        sweeper(jobs.clone(), cfg.sweep_interval),
        orphan_reconciler(jobs.clone(), cfg.orphan_interval),
    ];
    tasks.extend(worker_loop(jobs, handlers, cfg.worker_interval));
    let background = Background::start(tasks, liveness.clone());
    let stopper = background.stopper();
    state = state.with_liveness(liveness);

    let shutdown = shutdown_listener()?;
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "pair-api listening");
    let served = axum::serve(listener, pair_api::router(state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            stopper.stop();
        })
        .await;
    let report = background.drain(cfg.shutdown_drain).await;
    tracing::info!(drained = ?report.drained, aborted = ?report.aborted, "background tasks stopped");
    served?;
    tracing::info!("pair-api stopped");
    Ok(())
}
