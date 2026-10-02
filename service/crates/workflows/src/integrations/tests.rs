use super::disconnect::{apply_deletion_choice, disconnect, export};
use super::fake::{deleted, upsert, FakeClient};
use super::store::{self, AccountState};
use super::{attempt_write, ingest_account, DeletionChoice, ExternalWrite, Provider};
use crate::daily::testdb::TestDb;
use pair_core::error::ErrorCode;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn account(
    pool: &PgPool,
    provider: Provider,
    scopes: &[&str],
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = store::create_account(pool, provider).await?;
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    store::set_allowlist(pool, id, &scopes).await?;
    Ok(id)
}

async fn source_rows(pool: &PgPool, id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM integration_sources WHERE account_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
}

#[tokio::test]
async fn revoked_token_halts_ingestion() -> TestResult {
    let db = TestDb::new().await?;
    let id = account(&db.pool, Provider::Gmail, &["INBOX"]).await?;
    let client = FakeClient::default();
    client.push("INBOX", vec![upsert("m1", "r1", "hello")]);
    client.revoke();

    let err = ingest_account(&db.pool, &client, id)
        .await
        .err()
        .ok_or("ingestion ran with revoked token")?;
    assert_eq!(err.code, ErrorCode::Unauthenticated);
    assert_eq!(
        store::get_account(&db.pool, id).await?.state,
        AccountState::Revoked
    );

    let again = ingest_account(&db.pool, &client, id)
        .await
        .err()
        .ok_or("ingestion resumed")?;
    assert_eq!(again.code, ErrorCode::Unauthenticated);
    assert_eq!(
        client.calls().len(),
        1,
        "halted account must not call the provider again"
    );
    assert_eq!(source_rows(&db.pool, id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn stale_event_updated() -> TestResult {
    let db = TestDb::new().await?;
    let id = account(&db.pool, Provider::Calendar, &["primary"]).await?;
    let client = FakeClient::default();
    client.push(
        "primary",
        vec![upsert("ev1", "etag-1", "Design sync 11:00")],
    );
    ingest_account(&db.pool, &client, id).await?;

    client.push(
        "primary",
        vec![upsert("ev1", "etag-2", "Design sync 14:00")],
    );
    let stats = ingest_account(&db.pool, &client, id).await?;
    assert_eq!((stats.updated, stats.inserted), (1, 0));

    let bundle = export(&db.pool, id).await?;
    assert_eq!(bundle.sources.len(), 1);
    assert_eq!(bundle.sources[0].revision, "etag-2");
    assert_eq!(
        bundle.sources[0].content,
        Some(serde_json::json!({ "title": "Design sync 14:00" }))
    );
    Ok(())
}

#[tokio::test]
async fn deleted_message_tombstoned() -> TestResult {
    let db = TestDb::new().await?;
    let id = account(&db.pool, Provider::Gmail, &["INBOX"]).await?;
    let client = FakeClient::default();
    client.push("INBOX", vec![upsert("m1", "r1", "secret plans")]);
    ingest_account(&db.pool, &client, id).await?;
    client.push("INBOX", vec![deleted("m1")]);
    let stats = ingest_account(&db.pool, &client, id).await?;
    assert_eq!(stats.tombstoned, 1);

    let bundle = export(&db.pool, id).await?;
    assert_eq!(bundle.sources[0].state, "tombstoned");
    assert!(
        bundle.sources[0].content.is_none(),
        "tombstone must drop content"
    );
    assert_eq!(store::active_source_count(&db.pool, id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn duplicate_ingestion_idempotent() -> TestResult {
    let db = TestDb::new().await?;
    let id = account(&db.pool, Provider::Gmail, &["INBOX"]).await?;
    let client = FakeClient::default();
    client.push(
        "INBOX",
        vec![
            upsert("m1", "r1", "a"),
            upsert("m2", "r1", "b"),
            upsert("m2", "r1", "b"),
        ],
    );
    let first = ingest_account(&db.pool, &client, id).await?;
    let before = export(&db.pool, id).await?;

    // Force a full replay from the start.
    sqlx::query("DELETE FROM integration_cursors WHERE account_id = $1")
        .bind(id)
        .execute(&db.pool)
        .await?;
    let replay = ingest_account(&db.pool, &client, id).await?;
    assert_eq!((first.inserted, first.unchanged), (2, 1));
    assert_eq!(
        (replay.inserted, replay.updated, replay.tombstoned),
        (0, 0, 0)
    );
    assert_eq!(export(&db.pool, id).await?, before);
    assert_eq!(source_rows(&db.pool, id).await?, 2);
    Ok(())
}

#[tokio::test]
async fn disconnect_prevents_future_ingestion() -> TestResult {
    let db = TestDb::new().await?;
    let id = account(&db.pool, Provider::Gmail, &["INBOX"]).await?;
    let client = FakeClient::default();
    client.push("INBOX", vec![upsert("m1", "r1", "hello")]);
    ingest_account(&db.pool, &client, id).await?;
    let calls_before = client.calls().len();

    let report = disconnect(&db.pool, id).await?;
    assert_eq!(report.derived_source_count, 1);
    assert_eq!(
        report.choices,
        [
            DeletionChoice::KeepDerivedSources,
            DeletionChoice::DeleteDerivedSources
        ]
    );

    client.push("INBOX", vec![upsert("m2", "r1", "new mail")]);
    let err = ingest_account(&db.pool, &client, id)
        .await
        .err()
        .ok_or("ingested after disconnect")?;
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert_eq!(client.calls().len(), calls_before);

    assert_eq!(export(&db.pool, id).await?.sources.len(), 1);
    assert_eq!(
        apply_deletion_choice(&db.pool, id, DeletionChoice::DeleteDerivedSources).await?,
        1
    );
    assert_eq!(store::active_source_count(&db.pool, id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn writes_disabled_by_default() -> TestResult {
    let db = TestDb::new().await?;
    let id = store::create_account(&db.pool, Provider::Gmail).await?;
    let send = ExternalWrite::SendMessage {
        to: "a@example.com".into(),
        subject: "hi".into(),
    };
    let err = attempt_write(&db.pool, id, &send)
        .await
        .err()
        .ok_or("write allowed by default")?;
    assert_eq!(err.code, ErrorCode::PolicyDenied);

    // Enabling the flag still never yields an executed write.
    store::set_writes_enabled(&db.pool, id, true).await?;
    let err = attempt_write(&db.pool, id, &send)
        .await
        .err()
        .ok_or("write executed")?;
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    Ok(())
}

#[tokio::test]
async fn only_allowlisted_folders_read() -> TestResult {
    let db = TestDb::new().await?;
    let client = FakeClient::default();
    for scope in ["INBOX", "Projects", "Personal"] {
        client.push(scope, vec![upsert(&format!("{scope}-1"), "r1", scope)]);
    }

    // Default: nothing allowlisted, nothing read.
    let none = account(&db.pool, Provider::Gmail, &[]).await?;
    ingest_account(&db.pool, &client, none).await?;
    assert!(client.calls().is_empty());

    let id = account(&db.pool, Provider::Gmail, &["INBOX", "Projects"]).await?;
    ingest_account(&db.pool, &client, id).await?;
    let mut calls = client.calls();
    calls.sort();
    calls.dedup();
    assert_eq!(calls, ["INBOX", "Projects"]);
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT scope FROM integration_sources WHERE account_id = $1 ORDER BY scope",
    )
    .bind(id)
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(stored, ["INBOX", "Projects"]);
    Ok(())
}
