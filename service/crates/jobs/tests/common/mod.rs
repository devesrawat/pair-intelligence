#![allow(dead_code, clippy::unwrap_used)]
//! Per-test throwaway database: pair_t_jobs_<uuid>, migrated from the repo's migrations dir.
use pair_jobs::{JobConfig, JobStore};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

const DEFAULT_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";

pub struct TestDb {
    pub pool: PgPool,
    name: String,
    admin: PgConnectOptions,
}

fn admin_options() -> PgConnectOptions {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
    PgConnectOptions::from_str(&url).unwrap()
}

impl TestDb {
    pub async fn new() -> Self {
        let admin = admin_options();
        let name = format!("pair_t_jobs_{}", uuid::Uuid::new_v4().simple());
        let mut conn = PgConnection::connect_with(&admin).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect_with(admin.clone().database(&name))
            .await
            .unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        sqlx::migrate::Migrator::new(dir)
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        Self { pool, name, admin }
    }

    pub fn store(&self, cfg: JobConfig) -> JobStore {
        JobStore::new(self.pool.clone(), cfg)
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (admin, name) = (self.admin.clone(), self.name.clone());
        let handle = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                if let Ok(mut conn) = PgConnection::connect_with(&admin).await {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .execute(&mut conn)
                        .await;
                }
            });
        });
        let _ = handle.join();
    }
}

/// Short leases so crash tests do not wait a minute.
pub fn fast_cfg() -> JobConfig {
    JobConfig {
        lease_ttl: Duration::from_millis(300),
        ..JobConfig::default()
    }
}

pub async fn wait_lease_expiry() {
    tokio::time::sleep(Duration::from_millis(450)).await;
}
