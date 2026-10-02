//! Every crate tests only its own migrations; this proves the whole chain applies in order.
use sqlx::{migrate::Migrator, postgres::PgPoolOptions, Connection, Executor, PgConnection};
use std::path::Path;

const DEFAULT_DATABASE_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";

fn admin_url() -> String {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());
    match url.rsplit_once('/') {
        Some((base, _)) => format!("{base}/postgres"),
        None => url,
    }
}

#[tokio::test]
async fn full_migration_chain_applies_in_order() -> Result<(), Box<dyn std::error::Error>> {
    let name = format!("pair_t_chain_{}", uuid::Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&admin_url()).await?;
    admin
        .execute(format!("CREATE DATABASE {name}").as_str())
        .await?;

    let db_url = format!(
        "{}/{}",
        admin_url().rsplit_once('/').map_or("", |(b, _)| b),
        name
    );
    let result = async {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await?;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        let migrator = Migrator::new(dir).await?;
        migrator.run(&pool).await?;
        let applied: i64 =
            sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
                .fetch_one(&pool)
                .await?;
        pool.close().await;
        Ok::<i64, Box<dyn std::error::Error>>(applied)
    }
    .await;

    admin
        .execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
        .await?;
    let applied = result?;
    assert!(applied >= 11, "expected the full chain, applied {applied}");
    Ok(())
}
