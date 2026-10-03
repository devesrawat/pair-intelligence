//! Stuck `started` executions: a row that never reached a terminal outcome means the process
//! died (or the tool never returned) mid-call. The report is read-only.
mod common;

use chrono::Duration;
use common::*;
use ops::audit::{stuck_count, stuck_executions};

async fn insert(pool: &sqlx::PgPool, tool: &str, outcome: &str, age: &str) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tool_executions (id, task_id, trace_id, tool, args_hash, data_class, policy_version, decision, outcome, started_at, finished_at) \
         VALUES ($1, $2, $3, $4, repeat('a', 64), 'public', 'v1', 'allow', $5, now() - $6::interval, \
                 CASE WHEN $5 = 'started' THEN NULL ELSE now() END)",
    )
    .bind(id)
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(tool)
    .bind(outcome)
    .bind(age)
    .execute(pool)
    .await
    .expect("insert");
    id
}

#[tokio::test]
async fn stuck_executions_reports_only_old_started_rows() {
    let db = TestDb::create().await;
    let stuck = insert(&db.pool, "git.push", "started", "3 hours").await;
    insert(&db.pool, "fs.write", "started", "1 minute").await;
    insert(&db.pool, "fs.read", "ok", "5 hours").await;
    insert(&db.pool, "fs.read", "error", "5 hours").await;

    let rows = stuck_executions(&db.pool, Duration::minutes(60))
        .await
        .expect("report");

    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].id, stuck);
    assert_eq!(rows[0].tool, "git.push");
    assert!(rows[0].age_secs >= 3 * 3600 - 5, "{}", rows[0].age_secs);
    assert_eq!(
        stuck_count(&db.pool, Duration::minutes(60))
            .await
            .expect("count"),
        1
    );
    assert_eq!(
        stuck_count(&db.pool, Duration::hours(6))
            .await
            .expect("count"),
        0
    );
    let still: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tool_executions WHERE outcome = 'started'")
            .fetch_one(&db.pool)
            .await
            .expect("count");
    assert_eq!(still, 2, "the report never modifies rows");
    db.drop_db().await;
}

#[tokio::test]
async fn stuck_executions_orders_oldest_first() {
    let db = TestDb::create().await;
    let newer = insert(&db.pool, "a", "started", "2 hours").await;
    let older = insert(&db.pool, "b", "started", "9 hours").await;
    let rows = stuck_executions(&db.pool, Duration::minutes(1))
        .await
        .expect("report");
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![older, newer]
    );
    db.drop_db().await;
}

#[tokio::test]
async fn stuck_executions_rejects_non_positive_threshold() {
    let db = TestDb::create().await;
    assert!(stuck_executions(&db.pool, Duration::zero()).await.is_err());
    assert!(stuck_executions(&db.pool, Duration::minutes(-5))
        .await
        .is_err());
    db.drop_db().await;
}
