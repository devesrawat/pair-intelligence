//! Database-level invariants added by migrations 070-072: evidence cannot be stripped from a
//! memory, lifecycle columns stay consistent, audit outcomes are constrained, hot paths indexed.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, propose_verified, source, TestDb};
use pair_core::{traits::Memory, types::EvidenceRef, types::TrustClass};
use sqlx::PgPool;
use uuid::Uuid;

const PG_CHECK_VIOLATION: &str = "23514";

fn pg_code(err: &sqlx::Error) -> Option<String> {
    err.as_database_error()
        .and_then(|d| d.code().map(|c| c.to_string()))
}

async fn accepted_memory(db: &TestDb, ext: &str, content: &str) -> Uuid {
    let mem = db.memory();
    let src = source(&mem, ext, TrustClass::Owner).await;
    let cid = propose_verified(&mem, candidate("fact", content, None, src.id, "quoted"))
        .await
        .unwrap();
    mem.accept(cid, "owner").await.unwrap().0
}

async fn expect_check_violation(pool: &PgPool, sql: &str, id: Uuid) {
    let err = sqlx::query(sql).bind(id).execute(pool).await.unwrap_err();
    assert_eq!(
        pg_code(&err).as_deref(),
        Some(PG_CHECK_VIOLATION),
        "expected a check violation for: {sql}"
    );
}

#[tokio::test]
async fn deleting_last_evidence_row_fails_at_commit() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let a = source(&mem, "note-a", TrustClass::Owner).await;
    let b = source(&mem, "note-b", TrustClass::Owner).await;
    let mut c = candidate("fact", "Two sources agree", None, a.id, "span a");
    c.evidence.push(EvidenceRef {
        source: b.id,
        span: Some("span b".into()),
    });
    let id = mem
        .accept(propose_verified(&mem, c).await.unwrap(), "owner")
        .await
        .unwrap();

    // Removing one of two rows is fine.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM memory_evidence WHERE memory_id = $1 AND source_id = $2")
        .bind(id.0)
        .bind(a.id.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Removing the last one is rejected at commit.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM memory_evidence WHERE memory_id = $1")
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert_eq!(pg_code(&err).as_deref(), Some(PG_CHECK_VIOLATION));

    // Re-pointing the last row at another memory is the same violation.
    let other = accepted_memory(&db, "note-c", "Another memory").await;
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE memory_evidence SET memory_id = $2 WHERE memory_id = $1")
        .bind(id.0)
        .bind(other)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(tx.commit().await.is_err());
    assert_eq!(mem.get_memory(id).await.unwrap().evidence.len(), 1);
}

#[tokio::test]
async fn lifecycle_columns_must_be_consistent() {
    let db = TestDb::new().await;
    let id = accepted_memory(&db, "note-a", "A plain fact").await;
    expect_check_violation(
        &db.pool,
        "UPDATE memories SET status = 'superseded' WHERE id = $1",
        id,
    )
    .await;
    expect_check_violation(
        &db.pool,
        "UPDATE memories SET status = 'expired' WHERE id = $1",
        id,
    )
    .await;
    expect_check_violation(
        &db.pool,
        "UPDATE memories SET invalidated_reason = 'why' WHERE id = $1",
        id,
    )
    .await;
    // The legitimate forms are accepted.
    sqlx::query(
        "UPDATE memories SET status = 'expired', valid_to = valid_from, invalidated_reason = 'why' WHERE id = $1",
    )
    .bind(id)
    .execute(&db.pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn edited_candidate_requires_replacement() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "note-a", TrustClass::Owner).await;
    let cid = propose_verified(
        &mem,
        candidate("fact", "Draft statement", None, src.id, "q"),
    )
    .await
    .unwrap();
    expect_check_violation(
        &db.pool,
        "UPDATE memory_candidates SET state = 'edited' WHERE id = $1",
        cid.0,
    )
    .await;
}

#[tokio::test]
async fn audit_outcome_is_constrained() {
    let db = TestDb::new().await;
    for outcome in ["ok", "denied", "error"] {
        sqlx::query(
            "INSERT INTO memory_audit_events (id, actor, action, subject_kind, subject_id, outcome) \
             VALUES (gen_random_uuid(), 'a', 'x', 'memory', '1', $1)",
        )
        .bind(outcome)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    let err = sqlx::query(
        "INSERT INTO memory_audit_events (id, actor, action, subject_kind, subject_id, outcome) \
         VALUES (gen_random_uuid(), 'a', 'x', 'memory', '1', 'maybe')",
    )
    .execute(&db.pool)
    .await
    .unwrap_err();
    assert_eq!(pg_code(&err).as_deref(), Some(PG_CHECK_VIOLATION));
}

#[tokio::test]
async fn hot_path_indexes_exist() {
    let db = TestDb::new().await;
    for name in [
        "memory_chunks_source_idx",
        "memory_candidate_evidence_source_idx",
        "memory_candidates_pending_topic_idx",
    ] {
        let found: Option<String> =
            sqlx::query_scalar("SELECT indexname FROM pg_indexes WHERE indexname = $1")
                .bind(name)
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert!(found.is_some(), "missing index {name}");
    }
}

#[tokio::test]
async fn lifecycle_migration_tolerates_legacy_violations_and_rerun() {
    let db = TestDb::new().await;
    // Simulate a database that predates the constraint and already holds an offending audit row.
    sqlx::query(
        "ALTER TABLE memory_audit_events DROP CONSTRAINT memory_audit_events_outcome_check",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_audit_events (id, actor, action, subject_kind, subject_id, outcome) \
         VALUES (gen_random_uuid(), 'a', 'x', 'memory', '1', 'legacy-value')",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let migration = include_str!("../../../../migrations/071_memory_lifecycle_checks.sql");
    sqlx::raw_sql(migration).execute(&db.pool).await.unwrap();
    sqlx::raw_sql(migration).execute(&db.pool).await.unwrap();

    let validated: bool = sqlx::query_scalar(
        "SELECT convalidated FROM pg_constraint WHERE conname = 'memory_audit_events_outcome_check'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(!validated, "legacy row keeps the constraint NOT VALID");
    let err = sqlx::query(
        "INSERT INTO memory_audit_events (id, actor, action, subject_kind, subject_id, outcome) \
         VALUES (gen_random_uuid(), 'a', 'x', 'memory', '1', 'maybe')",
    )
    .execute(&db.pool)
    .await
    .unwrap_err();
    assert_eq!(pg_code(&err).as_deref(), Some(PG_CHECK_VIOLATION));
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conname IN ('memories_closed_has_valid_to', \
         'memories_invalidated_is_expired', 'memory_candidates_edited_has_replacement') AND convalidated",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(others, 3);
}
