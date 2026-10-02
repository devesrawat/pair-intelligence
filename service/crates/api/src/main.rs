//! pair-api binary: loads config, runs migrations (if any), serves HTTP.

use std::time::Duration;

use pair_api::config::Config;
use pair_api::state::AppState;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

const POOL_MAX_CONNECTIONS: u32 = 10;
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(POOL_MAX_CONNECTIONS)
        .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
        .connect(&cfg.database_url)
        .await?;

    if cfg.migrations_dir.is_dir() {
        Migrator::new(cfg.migrations_dir.as_path())
            .await?
            .run(&pool)
            .await?;
        tracing::info!(dir = %cfg.migrations_dir.display(), "migrations applied");
    }

    let state = AppState::new(pool, cfg.service_token.as_str())
        .with_migrations_dir(cfg.migrations_dir)
        .with_data_dir(cfg.data_dir);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "pair-api listening");
    axum::serve(listener, pair_api::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
