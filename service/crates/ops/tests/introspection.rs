//! Table introspection must look at what the unqualified purge SQL actually hits (search_path
//! resolution), and a purge that cannot run a CORE target is an error, never a quiet skip.
mod common;

use chrono::Utc;
use common::*;
use ops::retention::{purge, RetentionPolicy};
use ops::OpsError;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// A pool whose unqualified names resolve through `search_path` (what the purge SQL uses).
async fn pool_with_search_path(db: &TestDb, path: &str) -> PgPool {
    let opts = db
        .pool
        .connect_options()
        .as_ref()
        .clone()
        .options([("search_path", path)]);
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .expect("connect with search_path")
}

#[tokio::test]
async fn skipped_core_target_is_an_error_not_a_skip() {
    let db = TestDb::create().await;
    insert_model_call(&db.pool, 45, Some("secret"), None, None).await;
    // A decoy `model_calls` earlier on the search_path lacks the payload columns.
    sqlx::query("CREATE SCHEMA decoy")
        .execute(&db.pool)
        .await
        .expect("schema");
    sqlx::query("CREATE TABLE decoy.model_calls (id uuid PRIMARY KEY)")
        .execute(&db.pool)
        .await
        .expect("decoy table");
    let pool = pool_with_search_path(&db, "decoy,public").await;

    let err = purge(&pool, Utc::now(), &RetentionPolicy::default())
        .await
        .expect_err("a core target that cannot run must fail the purge");
    assert!(
        matches!(&err, OpsError::CoreTargetSkipped(names) if names.iter().any(|n| n == "model_calls.payloads")),
        "{err:?}"
    );
    let untouched: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public.model_calls WHERE request_payload IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(untouched, 1, "nothing is erased when the run is refused");
    pool.close().await;
    db.drop_db().await;
}

#[tokio::test]
async fn introspection_follows_the_table_the_sql_resolves_to() {
    let db = TestDb::create().await;
    insert_model_call(&db.pool, 45, Some("secret"), None, None).await;
    // The first schema on the path exists but holds no `model_calls`: the SQL resolves to
    // public.model_calls, so the target must run (current_schema() would have said "missing").
    sqlx::query("CREATE SCHEMA decoy")
        .execute(&db.pool)
        .await
        .expect("schema");
    let pool = pool_with_search_path(&db, "decoy,public").await;

    let report = purge(&pool, Utc::now(), &RetentionPolicy::default())
        .await
        .expect("purge");
    assert_eq!(report.erased("model_calls.payloads"), 1);
    assert!(!report.skipped.iter().any(|s| s.starts_with("model_calls")));
    pool.close().await;
    db.drop_db().await;
}
