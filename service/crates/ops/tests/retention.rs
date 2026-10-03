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
async fn purge_skips_tool_executions_when_table_absent() {
    let db = TestDb::create().await;
    sqlx::query("DROP TABLE tool_executions")
        .execute(&db.pool)
        .await
        .expect("drop");
    let report = purge(&db.pool, Utc::now(), &policy())
        .await
        .expect("purge without table");
    assert!(report
        .skipped
        .contains(&"tool_executions.payloads".to_owned()));
    db.drop_db().await;
}

async fn insert_tool_execution(
    pool: &sqlx::PgPool,
    age: &str,
    dest: &str,
    outcome: &str,
) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tool_executions (id, task_id, trace_id, tool, args_hash, destination, data_class, policy_version, decision, outcome, started_at, finished_at) \
         VALUES ($1, $2, $2, 'web.fetch', repeat('a', 64), $3, 'public', 'v1', 'allow', $4, now() - $5::interval, \
                 CASE WHEN $4 = 'started' THEN NULL ELSE now() - $5::interval END)",
    )
    .bind(id)
    .bind(uuid::Uuid::new_v4())
    .bind(dest)
    .bind(outcome)
    .bind(age)
    .execute(pool)
    .await
    .expect("insert tool execution");
    id
}

/// The REAL `tool_executions` (migration 090) stores only an args hash and the (sanitized)
/// destination. The destination is egress audit evidence, so purge must leave every row intact.
#[tokio::test]
async fn purge_keeps_destinations_of_old_tool_executions_on_the_real_schema() {
    let db = TestDb::create().await;
    insert_tool_execution(&db.pool, "45 days", "https://example.org/a", "ok").await;
    insert_tool_execution(&db.pool, "1 day", "https://example.org/b", "ok").await;

    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("tool_executions.payloads"), 0);
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tool_executions WHERE destination LIKE 'https://example.org/%'",
    )
    .fetch_one(&db.pool)
    .await
    .expect("count");
    assert_eq!(kept, 2, "egress audit evidence must survive the purge");
    db.drop_db().await;
}

/// A table from a variant schema that does carry payload columns: unfinished rows (the process
/// died mid-call) are evidence of an incomplete action and are never erased.
#[tokio::test]
async fn purge_never_erases_started_tool_executions() {
    let db = TestDb::create().await;
    sqlx::query("ALTER TABLE tool_executions ADD COLUMN stdout text")
        .execute(&db.pool)
        .await
        .expect("variant column");
    let started = insert_tool_execution(&db.pool, "45 days", "h", "started").await;
    let done = insert_tool_execution(&db.pool, "45 days", "h", "ok").await;
    sqlx::query("UPDATE tool_executions SET stdout = 'output'")
        .execute(&db.pool)
        .await
        .expect("fill");

    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("tool_executions.payloads"), 1);
    let out = |id| {
        let pool = db.pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT stdout FROM tool_executions WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("row")
        }
    };
    assert_eq!(out(started).await.as_deref(), Some("output"));
    assert_eq!(out(done).await, None);
    db.drop_db().await;
}

#[tokio::test]
async fn purge_truncated_is_false_when_batches_exactly_cover_the_work() {
    let db = TestDb::create().await;
    for _ in 0..4 {
        insert_model_call(&db.pool, 45, Some("p"), None, None).await;
    }
    let exact = RetentionPolicy {
        batch_size: 2,
        max_batches_per_target: 2,
        ..RetentionPolicy::default()
    };
    let report = purge(&db.pool, Utc::now(), &exact).await.expect("purge");
    assert_eq!(report.erased("model_calls.payloads"), 4);
    assert!(
        !report.truncated,
        "nothing remains, so nothing is truncated"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn purge_truncated_is_true_when_skip_locked_leaves_rows_behind() {
    let db = TestDb::create().await;
    let locked = insert_model_call(&db.pool, 45, Some("p"), None, None).await;
    insert_model_call(&db.pool, 45, Some("p"), None, None).await;
    let mut holder = db.pool.begin().await.expect("begin");
    sqlx::query("SELECT id FROM model_calls WHERE id = $1 FOR UPDATE")
        .bind(locked)
        .execute(&mut *holder)
        .await
        .expect("lock one row");

    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(
        report.erased("model_calls.payloads"),
        1,
        "the locked row is skipped"
    );
    assert!(
        report.truncated,
        "a skipped row means work remains: run again"
    );

    holder.rollback().await.expect("release");
    let rest = purge(&db.pool, Utc::now(), &policy()).await.expect("again");
    assert_eq!(rest.erased("model_calls.payloads"), 1);
    assert!(!rest.truncated);
    db.drop_db().await;
}

#[tokio::test]
async fn research_purge_erases_claim_spans_and_reports_of_purged_sources() {
    let db = TestDb::create().await;
    let run = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO research_runs (id, question, scope) VALUES ($1, 'q', '{}'::jsonb)")
        .bind(run)
        .execute(&db.pool)
        .await
        .expect("run");
    sqlx::query(
        "INSERT INTO research_reports (run_id, markdown) VALUES ($1, 'REPORT quotes SOURCE TEXT')",
    )
    .bind(run)
    .execute(&db.pool)
    .await
    .expect("report");
    let claim = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO research_claims (id, run_id, topic, claim_text, claim_value, status) VALUES ($1, $2, 't', 'c', 'v', 'validated')")
        .bind(claim)
        .bind(run)
        .execute(&db.pool)
        .await
        .expect("claim");
    let mut sources = Vec::new();
    for (url, age, pinned) in [
        ("https://a", 120, false),
        ("https://b", 120, true),
        ("https://c", 5, false),
    ] {
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO research_sources (id, run_id, url, normalized_url, available, content_sha256, fetched_at, text, pinned) VALUES ($1, $2, $3, $3, true, 'h', now() - make_interval(days => $4::int), 'SOURCE TEXT', $5)")
            .bind(id).bind(run).bind(url).bind(age).bind(pinned)
            .execute(&db.pool).await.expect("source");
        sqlx::query("INSERT INTO research_claim_evidence (id, claim_id, source_id, cited_url, span) VALUES ($1, $2, $3, $4, $5)")
            .bind(uuid::Uuid::new_v4()).bind(claim).bind(id).bind(url).bind(format!("SPAN of {url}"))
            .execute(&db.pool).await.expect("evidence");
        sources.push(url);
    }

    let report = purge(&db.pool, Utc::now(), &policy()).await.expect("purge");
    assert_eq!(report.erased("research_sources.text"), 1);
    assert_eq!(report.erased("research_claim_evidence.span"), 1);
    assert_eq!(report.erased("research_reports.markdown"), 1);

    let spans: Vec<String> =
        sqlx::query_scalar("SELECT span FROM research_claim_evidence ORDER BY cited_url")
            .fetch_all(&db.pool)
            .await
            .expect("spans");
    assert_eq!(spans.len(), 3, "evidence rows (and their source ids) stay");
    assert!(
        !spans[0].contains("SPAN of"),
        "purged source: span erased, got {}",
        spans[0]
    );
    assert_eq!(
        spans[1], "SPAN of https://b",
        "pinned source keeps its span"
    );
    assert_eq!(spans[2], "SPAN of https://c", "fresh source keeps its span");
    let markdown: String = sqlx::query_scalar("SELECT markdown FROM research_reports")
        .fetch_one(&db.pool)
        .await
        .expect("report");
    assert!(!markdown.contains("SOURCE TEXT"));
    let again = purge(&db.pool, Utc::now(), &policy()).await.expect("again");
    assert_eq!(again.total(), 0, "idempotent: {again:?}");
    db.drop_db().await;
}
