//! Test harness: one uniquely named, fully migrated database per test, force-dropped afterwards.
//! Never touches the shared `pair` database.
use pair_budget::{BudgetConfig, PgBudget, PriceBook, MIGRATOR};
use sqlx::PgPool;

const DEFAULT_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";
const ADMIN_DB: &str = "postgres";

pub struct TestDb {
    pub pool: PgPool,
    name: String,
    admin_url: String,
}

fn url_for(db: &str) -> String {
    let base = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
    let mut u = url::Url::parse(&base).expect("DATABASE_URL parses");
    u.set_path(db);
    u.to_string()
}

/// Budget YAML with caps given in cents (the loader accepts two decimal places).
pub fn budget_yaml(month_c: i64, day_c: i64, classifier_c: i64, task_c: i64) -> String {
    let f = |c: i64| format!("{}.{:02}", c / 100, c % 100);
    format!(
        "budget:\n  currency: USD\n  metered_monthly_cap: {}\n  metered_daily_cap: {}\n  \
         classifier_monthly_subcap: {}\n  default_task_cap: {}\n  research_task_cap: {}\n  \
         coding_task_cap: {}\n  auto_top_up: false\nschedule:\n  timezone: Asia/Kolkata\n",
        f(month_c),
        f(day_c),
        f(classifier_c),
        f(task_c),
        f(task_c),
        f(task_c)
    )
}

impl TestDb {
    pub async fn create() -> Self {
        let name = format!("pair_t_models_{}", uuid::Uuid::new_v4().simple());
        let admin_url = url_for(ADMIN_DB);
        let admin = PgPool::connect(&admin_url).await.expect("connect admin");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create test db");
        admin.close().await;
        let pool = PgPool::connect(&url_for(&name)).await.expect("connect");
        MIGRATOR.run(&pool).await.expect("migrate");
        Self {
            pool,
            name,
            admin_url,
        }
    }

    /// Real `PgBudget` with the given cents-denominated caps; `price_version` is the current version.
    pub fn budget(&self, yaml: &str, price_version: &str) -> PgBudget {
        let cfg = BudgetConfig::from_yaml(yaml).expect("budget yaml");
        PgBudget::new(
            self.pool.clone(),
            cfg,
            PriceBook::new(Some(price_version.to_owned()), []),
        )
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (admin_url, name) = (self.admin_url.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok(admin) = PgPool::connect(&admin_url).await {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .execute(&admin)
                        .await;
                    admin.close().await;
                }
            });
        })
        .join();
    }
}
