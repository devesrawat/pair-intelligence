//! Migrations 100-119 applied on top of the full chain (see tests/common).
mod common;

use common::*;
use sqlx::PgPool;
use uuid::Uuid;

async fn exists(pool: &PgPool, table: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("to_regclass")
}

fn is_fk_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23503")
}

#[tokio::test]
async fn schema_new_spec7_tables_exist() {
    let db = TestDb::create().await;
    for t in ["projects", "eval_cases", "eval_results"] {
        assert!(exists(&db.pool, t).await, "{t} missing");
    }
    db.drop_db().await;
}

#[tokio::test]
async fn schema_projects_name_unique_and_defaults_apply() {
    let db = TestDb::create().await;
    sqlx::query("INSERT INTO projects (name, owner) VALUES ('pair', 'devesh')")
        .execute(&db.pool)
        .await
        .expect("insert with defaults");
    let (status, has_id): (String, bool) =
        sqlx::query_as("SELECT status, id IS NOT NULL FROM projects WHERE name = 'pair'")
            .fetch_one(&db.pool)
            .await
            .expect("row");
    assert_eq!(status, "active");
    assert!(has_id);
    let dup = sqlx::query("INSERT INTO projects (name, owner) VALUES ('pair', 'other')")
        .execute(&db.pool)
        .await;
    assert!(dup.is_err(), "duplicate project name must be rejected");
    db.drop_db().await;
}

#[tokio::test]
async fn schema_eval_results_reject_unknown_case_and_bad_split() {
    let db = TestDb::create().await;
    let orphan = sqlx::query("INSERT INTO eval_results (case_id, config_version, model, score) VALUES ($1, 'c1', 'm', 0.5)")
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await
        .expect_err("orphan result rejected");
    assert!(is_fk_violation(&orphan));

    let bad = sqlx::query("INSERT INTO eval_cases (dataset, version, case_id, input, expected, split) VALUES ('d', 'v1', 'c1', '{}', '{}', 'train')")
        .execute(&db.pool)
        .await;
    assert!(bad.is_err(), "split must be dev|held_out");

    let case: Uuid = sqlx::query_scalar("INSERT INTO eval_cases (dataset, version, case_id, input, expected, split) VALUES ('d', 'v1', 'c1', '{}', '{}', 'held_out') RETURNING id")
        .fetch_one(&db.pool)
        .await
        .expect("case");
    let label: String = sqlx::query_scalar("SELECT label_status FROM eval_cases WHERE id = $1")
        .bind(case)
        .fetch_one(&db.pool)
        .await
        .expect("label");
    assert_eq!(label, "draft_unreviewed", "labels default to unreviewed");
    sqlx::query("INSERT INTO eval_results (case_id, config_version, model, score) VALUES ($1, 'c1', 'm', 0.5)")
        .bind(case)
        .execute(&db.pool)
        .await
        .expect("result");
    let del = sqlx::query("DELETE FROM eval_cases WHERE id = $1")
        .bind(case)
        .execute(&db.pool)
        .await
        .expect_err("case with results cannot be deleted");
    assert!(is_fk_violation(&del));
    db.drop_db().await;
}

#[tokio::test]
async fn schema_model_calls_reservation_fk_rejects_bad_ids_and_restricts_delete() {
    let db = TestDb::create().await;
    let bad = insert_model_call_result(&db.pool, Some(Uuid::new_v4())).await;
    assert!(is_fk_violation(
        &bad.expect_err("bad reservation id rejected")
    ));
    insert_model_call_result(&db.pool, None)
        .await
        .expect("nullable reservation ok");
    let res = insert_reservation(&db.pool, "settled").await;
    insert_model_call_result(&db.pool, Some(res))
        .await
        .expect("valid reservation ok");
    let del = sqlx::query("DELETE FROM budget_reservations WHERE id = $1")
        .bind(res)
        .execute(&db.pool)
        .await
        .expect_err("RESTRICT blocks delete");
    assert!(is_fk_violation(&del));
    db.drop_db().await;
}

async fn insert_model_call_result(
    pool: &PgPool,
    reservation: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO model_calls (id, task_id, trace_id, reservation_id, provider, requested_model, cost_state, route_reason, latency_ms, status) \
         VALUES ($1, $1, $1, $2, 'p', 'm', 'unpriced', 'r', 1, 'ok')",
    )
    .bind(Uuid::new_v4())
    .bind(reservation)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn insert_run(pool: &PgPool, approval: Option<Uuid>) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflow_runs (id, idempotency_key, kind, input, input_hash, run_class, state, deadline_at, approval_id) \
         VALUES ($1, $1, 'k', '{}', 'h', 'interactive', 'queued', now() + interval '1 hour', $2)",
    )
    .bind(id)
    .bind(approval)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn insert_approval(pool: &PgPool, consumed_by: Option<Uuid>) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO approvals (id, action_hash, actor, expires_at, consumed_by) VALUES ($1, 'h', 'owner', now() + interval '1 hour', $2)",
    )
    .bind(id)
    .bind(consumed_by)
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test]
async fn schema_workflow_run_approval_and_consumed_by_fks_reject_bad_ids() {
    let db = TestDb::create().await;
    let bad_run = insert_run(&db.pool, Some(Uuid::new_v4()))
        .await
        .expect_err("bad approval id");
    assert!(is_fk_violation(&bad_run));
    let bad_approval = insert_approval(&db.pool, Some(Uuid::new_v4()))
        .await
        .expect_err("bad consumed_by");
    assert!(is_fk_violation(&bad_approval));

    let approval = insert_approval(&db.pool, None).await.expect("approval");
    let run = insert_run(&db.pool, Some(approval))
        .await
        .expect("run references real approval");
    sqlx::query("UPDATE approvals SET consumed_at = now(), consumed_by = $2 WHERE id = $1")
        .bind(approval)
        .bind(run)
        .execute(&db.pool)
        .await
        .expect("consumed_by references real run");
    db.drop_db().await;
}

#[tokio::test]
async fn schema_approval_and_audit_columns_default_safely() {
    let db = TestDb::create().await;
    let approval = insert_approval(&db.pool, None)
        .await
        .expect("insert without new columns");
    let (scope, decision): (Option<String>, String) =
        sqlx::query_as("SELECT scope, decision FROM approvals WHERE id = $1")
            .bind(approval)
            .fetch_one(&db.pool)
            .await
            .expect("row");
    assert_eq!(scope, None);
    assert_eq!(decision, "granted");
    let bad = sqlx::query("UPDATE approvals SET decision = 'maybe' WHERE id = $1")
        .bind(approval)
        .execute(&db.pool)
        .await;
    assert!(bad.is_err());

    // A writer from before this migration (no policy_version / outcome) still works.
    sqlx::query("INSERT INTO audit_events (id, trace_id, actor, kind, subject) VALUES ($1, $1, 'a', 'k', 's')")
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await
        .expect("old-shape audit insert");
    let bad = sqlx::query("INSERT INTO audit_events (id, trace_id, actor, kind, subject, outcome) VALUES ($1, $1, 'a', 'k', 's', 'weird')")
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await;
    assert!(bad.is_err(), "outcome is constrained");
    sqlx::query("INSERT INTO audit_events (id, trace_id, actor, kind, subject, policy_version, outcome) VALUES ($1, $1, 'a', 'k', 's', 'p1', 'denied')")
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await
        .expect("new columns accepted");
    db.drop_db().await;
}

#[tokio::test]
async fn schema_integration_source_insert_works_without_source_updated_at() {
    let db = TestDb::create().await;
    sqlx::query("INSERT INTO integration_accounts (id, provider) VALUES ('00000000-0000-0000-0000-000000000009', 'gmail')")
        .execute(&db.pool)
        .await
        .expect("account");
    sqlx::query("INSERT INTO integration_sources (account_id, external_id, scope, kind, revision) VALUES ('00000000-0000-0000-0000-000000000009', 'e', 's', 'message', 'r')")
        .execute(&db.pool)
        .await
        .expect("pre-051 style insert must work");
    db.drop_db().await;
}

#[tokio::test]
async fn schema_retention_columns_default_unpinned() {
    let db = TestDb::create().await;
    let id = insert_source(&db.pool, 1, false, "u").await;
    let pinned: bool = sqlx::query_scalar("SELECT pinned FROM sources WHERE id = $1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .expect("pinned");
    assert!(!pinned);
    db.drop_db().await;
}
