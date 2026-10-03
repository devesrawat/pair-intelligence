mod common;

use chrono::Utc;
use common::*;
use ops::retention::{purge, RetentionPolicy};

fn policy() -> RetentionPolicy {
    RetentionPolicy::default()
}

#[tokio::test]
async fn purge_erases_old_model_payloads_keeps_rows() {
    let db = TestDb::create().await;
    let old = insert_model_call(
        &db.pool,
        45,
        Some("secret prompt"),
        Some("secret answer"),
        None,
    )
    .await;
    let fresh = insert_model_call(
        &db.pool,
        5,
        Some("recent prompt"),
        Some("recent answer"),
        None,
    )
    .await;

    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("model_calls.payloads"), 1);

    let (req, resp, purged, status): (Option<String>, Option<String>, Option<chrono::DateTime<Utc>>, String) =
        sqlx::query_as("SELECT request_payload, response_payload, payload_purged_at, status FROM model_calls WHERE id = $1")
            .bind(old)
            .fetch_one(&db.pool)
            .await
            .expect("old row still exists");
    assert!(
        req.is_none() && resp.is_none(),
        "content erased, not flagged"
    );
    assert!(purged.is_some());
    assert_eq!(status, "ok", "non-payload columns are kept");

    let kept: Option<String> =
        sqlx::query_scalar("SELECT request_payload FROM model_calls WHERE id = $1")
            .bind(fresh)
            .fetch_one(&db.pool)
            .await
            .expect("fresh row");
    assert_eq!(kept.as_deref(), Some("recent prompt"));
    db.drop_db().await;
}

#[tokio::test]
async fn pinned_sources_survive_90_days() {
    let db = TestDb::create().await;
    let pinned = insert_source(&db.pool, 120, true, "file:///pinned").await;
    let old = insert_source(&db.pool, 120, false, "file:///old").await;
    let young = insert_source(&db.pool, 30, false, "file:///young").await;

    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("sources.raw"), 1);

    let uri = |id| {
        let pool = db.pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT uri FROM sources WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("source row kept")
        }
    };
    assert_eq!(uri(pinned).await.as_deref(), Some("file:///pinned"));
    assert_eq!(uri(young).await.as_deref(), Some("file:///young"));
    assert_eq!(uri(old).await, None);
    db.drop_db().await;
}

#[tokio::test]
async fn purge_erases_raw_integration_and_research_content_unless_pinned() {
    let db = TestDb::create().await;
    sqlx::query("INSERT INTO integration_accounts (id, provider) VALUES ('00000000-0000-0000-0000-000000000001', 'gmail')")
        .execute(&db.pool)
        .await
        .expect("account");
    for (ext, pinned) in [("old", false), ("pinned", true)] {
        sqlx::query(
            "INSERT INTO integration_sources (account_id, external_id, scope, kind, revision, content, updated_at, pinned) \
             VALUES ('00000000-0000-0000-0000-000000000001', $1, 's', 'message', 'r', '{\"body\":\"hello\"}'::jsonb, now() - interval '100 days', $2)",
        )
        .bind(ext)
        .bind(pinned)
        .execute(&db.pool)
        .await
        .expect("integration source");
    }
    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("integration_sources.content"), 1);
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM integration_sources WHERE content IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .expect("count");
    assert_eq!(remaining, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn decisions_never_purged_by_age() {
    let db = TestDb::create().await;
    let src = insert_source(&db.pool, 400, false, "file:///ancient").await;
    let decision = insert_memory(
        &db.pool,
        "decision",
        "Use Postgres for state",
        src,
        "we chose postgres",
        400,
    )
    .await;

    purge(&db.pool, Utc::now(), &policy()).await.expect("purge");

    let content: String = sqlx::query_scalar("SELECT content FROM memories WHERE id = $1")
        .bind(decision)
        .fetch_one(&db.pool)
        .await
        .expect("memory kept");
    assert_eq!(content, "Use Postgres for state");
    let span: Option<String> =
        sqlx::query_scalar("SELECT span FROM memory_evidence WHERE memory_id = $1")
            .bind(decision)
            .fetch_one(&db.pool)
            .await
            .expect("evidence kept");
    assert_eq!(span.as_deref(), Some("we chose postgres"));
    db.drop_db().await;
}

#[tokio::test]
async fn rejected_candidate_spans_are_erased() {
    let db = TestDb::create().await;
    let src = insert_source(&db.pool, 200, false, "file:///x").await;
    let cand = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO memory_candidates (id, kind, content, normalized_content, dedupe_key, state) VALUES ($1, 'fact', 'c', 'c', $1::text, 'rejected')")
        .bind(cand)
        .execute(&db.pool)
        .await
        .expect("candidate");
    for span in ["span one", "span two"] {
        sqlx::query("INSERT INTO memory_candidate_evidence (candidate_id, source_id, span) VALUES ($1, $2, $3)")
            .bind(cand)
            .bind(src)
            .bind(span)
            .execute(&db.pool)
            .await
            .expect("candidate evidence");
    }
    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("memory_candidate_evidence.spans"), 2);
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_candidate_evidence WHERE span LIKE 'span %'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(leaked, 0);
    db.drop_db().await;
}

#[tokio::test]
async fn purge_is_idempotent() {
    let db = TestDb::create().await;
    insert_model_call(&db.pool, 45, Some("p"), Some("r"), None).await;
    insert_source(&db.pool, 120, false, "file:///old").await;

    let first = purge(&db.pool, Utc::now(), &policy()).await.expect("first");
    assert!(first.total() >= 2);
    let second = purge(&db.pool, Utc::now(), &policy())
        .await
        .expect("second");
    assert_eq!(second.total(), 0, "nothing left to erase: {second:?}");
    assert!(!second.truncated);
    db.drop_db().await;
}

#[tokio::test]
async fn purge_respects_batch_limit() {
    let db = TestDb::create().await;
    for _ in 0..5 {
        insert_model_call(&db.pool, 45, Some("p"), None, None).await;
    }
    let limited = RetentionPolicy {
        batch_size: 2,
        max_batches_per_target: 1,
        ..RetentionPolicy::default()
    };
    let first = purge(&db.pool, Utc::now(), &limited).await.expect("first");
    assert_eq!(first.erased("model_calls.payloads"), 2);
    assert!(first.truncated, "work remains, the report must say so");

    let roomy = RetentionPolicy {
        batch_size: 2,
        max_batches_per_target: 10,
        ..RetentionPolicy::default()
    };
    let rest = purge(&db.pool, Utc::now(), &roomy).await.expect("rest");
    assert_eq!(rest.erased("model_calls.payloads"), 3);
    assert!(!rest.truncated);
    db.drop_db().await;
}

#[tokio::test]
async fn purge_dry_run_counts_without_erasing() {
    let db = TestDb::create().await;
    insert_model_call(&db.pool, 45, Some("p"), Some("r"), None).await;
    let dry = RetentionPolicy {
        dry_run: true,
        ..RetentionPolicy::default()
    };
    let report = purge(&db.pool, Utc::now(), &dry).await.expect("dry run");
    assert!(report.dry_run);
    assert_eq!(report.erased("model_calls.payloads"), 1);
    let still: i64 =
        sqlx::query_scalar("SELECT count(*) FROM model_calls WHERE request_payload IS NOT NULL")
            .fetch_one(&db.pool)
            .await
            .expect("count");
    assert_eq!(still, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn purge_never_touches_unresolved_budget_reservations() {
    let db = TestDb::create().await;
    let held = insert_reservation(&db.pool, "held").await;
    let unresolved = insert_reservation(&db.pool, "unresolved").await;
    let settled = insert_reservation(&db.pool, "settled").await;
    let call_held = insert_model_call(&db.pool, 45, Some("p"), Some("r"), Some(held)).await;
    let call_settled = insert_model_call(&db.pool, 45, Some("p"), Some("r"), Some(settled)).await;
    let before: Vec<(uuid::Uuid, String, i64)> =
        sqlx::query_as("SELECT id, state, counted_micros FROM budget_reservations ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .expect("before");

    purge(&db.pool, Utc::now(), &policy()).await.expect("purge");

    let after: Vec<(uuid::Uuid, String, i64)> =
        sqlx::query_as("SELECT id, state, counted_micros FROM budget_reservations ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .expect("after");
    assert_eq!(before, after, "reservations are never modified or deleted");
    assert!(after
        .iter()
        .any(|r| r.0 == unresolved && r.1 == "unresolved"));

    let payload = |id| {
        let pool = db.pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT request_payload FROM model_calls WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("row")
        }
    };
    assert!(
        payload(call_held).await.is_some(),
        "evidence for an unresolved charge is retained"
    );
    assert!(payload(call_settled).await.is_none());
    db.drop_db().await;
}

#[tokio::test]
async fn purge_detects_and_erases_tool_executions_when_table_exists() {
    let db = TestDb::create().await;
    let absent = purge(&db.pool, Utc::now(), &policy())
        .await
        .expect("purge without table");
    assert!(absent
        .skipped
        .contains(&"tool_executions.payloads".to_owned()));

    sqlx::query("CREATE TABLE tool_executions (id uuid PRIMARY KEY, tool text NOT NULL, args jsonb NOT NULL, stdout text, created_at timestamptz NOT NULL)")
        .execute(&db.pool)
        .await
        .expect("create table");
    sqlx::query("INSERT INTO tool_executions VALUES ($1, 'git', '{\"x\":1}', 'out', now() - interval '45 days'), ($2, 'git', '{\"x\":2}', 'out', now())")
        .bind(uuid::Uuid::new_v4())
        .bind(uuid::Uuid::new_v4())
        .execute(&db.pool)
        .await
        .expect("rows");
    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("tool_executions.payloads"), 1);
    let (erased, kept): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE stdout IS NULL AND args = 'null'::jsonb), count(*) FILTER (WHERE stdout = 'out') FROM tool_executions",
    )
    .fetch_one(&db.pool)
    .await
    .expect("counts");
    assert_eq!((erased, kept), (1, 1));
    let again = purge(&db.pool, Utc::now(), &policy()).await.expect("again");
    assert_eq!(again.erased("tool_executions.payloads"), 0);
    db.drop_db().await;
}
