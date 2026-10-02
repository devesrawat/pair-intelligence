//! Per-test throwaway database on the shared Postgres. Test-only.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use sqlx::{postgres::PgPoolOptions, PgPool};

const DEFAULT_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";
const MIGRATION: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../migrations/040_research_evidence.sql"
);

pub struct TestDb {
    pub pool: PgPool,
    name: String,
    admin_url: String,
}

fn base_url() -> String {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let url = url.split('?').next().unwrap_or_default().to_string();
    url.rsplit_once('/').map(|(b, _)| b.to_string()).unwrap()
}

impl TestDb {
    pub async fn create() -> Self {
        let base = base_url();
        let admin_url = format!("{base}/postgres");
        let name = format!("pair_t_wf_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&format!("{base}/{name}"))
            .await
            .unwrap();
        let sql = std::fs::read_to_string(MIGRATION).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        Self {
            pool,
            name,
            admin_url,
        }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (name, admin_url) = (self.name.clone(), self.admin_url.clone());
        let handle = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok(admin) = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&admin_url)
                    .await
                {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&admin)
                            .await;
                }
            });
        });
        let _ = handle.join();
    }
}
