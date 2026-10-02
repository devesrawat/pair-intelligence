//! Test harness: one uniquely named database per test, migrated and dropped afterwards.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
use pair_budget::{BudgetConfig, PgBudget, PriceBook, MIGRATOR};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use std::str::FromStr;

const DEFAULT_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";
pub const PRICE: &str = "p-test-1";

pub struct TestDb {
    pub pool: PgPool,
    name: String,
    admin_url: String,
}

fn admin_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned())
}

impl TestDb {
    pub async fn create() -> Self {
        let admin_url = admin_url();
        let name = format!("pair_t_budget_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnectOptions::from_str(&admin_url).unwrap().connect().await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}")).execute(&mut admin).await.unwrap();
        let opts = PgConnectOptions::from_str(&admin_url).unwrap().database(&name);
        let pool = PgPoolOptions::new().max_connections(30).connect_with(opts).await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        Self { pool, name, admin_url }
    }

    pub fn budget(&self, yaml: &str) -> PgBudget {
        let cfg = BudgetConfig::from_yaml(yaml).unwrap();
        PgBudget::new(self.pool.clone(), cfg, PriceBook::new(Some(PRICE.to_owned()), []))
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (url, name) = (self.admin_url.clone(), self.name.clone());
        let handle = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(async {
                let Ok(opts) = PgConnectOptions::from_str(&url) else { return };
                if let Ok(mut admin) = opts.connect().await {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .execute(&mut admin)
                        .await;
                }
            });
        });
        let _ = handle.join();
    }
}

/// Config YAML with caps given in cents (avoids float formatting in tests).
pub fn yaml(month_c: i64, day_c: i64, classifier_c: i64, task_c: i64) -> String {
    let f = |c: i64| format!("{}.{:02}", c / 100, c % 100);
    format!(
        "budget:\n  currency: USD\n  metered_monthly_cap: {}\n  metered_daily_cap: {}\n  \
         classifier_monthly_subcap: {}\n  default_task_cap: {}\n  research_task_cap: {}\n  \
         coding_task_cap: {}\n  auto_top_up: false\nschedule:\n  timezone: Asia/Kolkata\n",
        f(month_c), f(day_c), f(classifier_c), f(task_c), f(task_c), f(task_c)
    )
}
