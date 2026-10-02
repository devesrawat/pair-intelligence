//! Per-test throwaway database on the shared Postgres. Never touches the `pair` database.
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use std::path::Path;
use uuid::Uuid;

const DEFAULT_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";
const ADMIN_DB: &str = "postgres";

pub(crate) struct TestDb {
    pub pool: PgPool,
    admin_url: String,
    name: String,
}

fn base_url() -> String {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    match url.rsplit_once('/') {
        Some((base, _db)) => base.to_string(),
        None => url,
    }
}

impl TestDb {
    pub(crate) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let base = base_url();
        let admin_url = format!("{base}/{ADMIN_DB}");
        let name = format!("pair_t_daily_{}", Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(&admin_url).await?;
        sqlx::query(&format!("CREATE DATABASE {name}")).execute(&mut admin).await?;
        admin.close().await?;
        let pool = PgPoolOptions::new().max_connections(4).connect(&format!("{base}/{name}")).await?;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        Migrator::new(dir).await?.run(&pool).await?;
        Ok(Self { pool, admin_url, name })
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (url, name) = (self.admin_url.clone(), self.name.clone());
        let cleanup = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async {
                if let Ok(mut c) = PgConnection::connect(&url).await {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).execute(&mut c).await;
                }
            });
        });
        let _ = cleanup.join();
    }
}
