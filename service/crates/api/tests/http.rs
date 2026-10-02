//! Contract tests for auth, tracing, liveness and readiness.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{authed, dead_pool, send, unique_dir, TestDb, TOKEN};
use pair_api::providers::{ProviderHealth, ProviderStatus};
use pair_api::state::AppState;
use pair_telemetry::health::{DiskStats, BYTES_PER_GIB};

fn app_with(state: AppState) -> axum::Router {
    pair_api::router(state)
}

/// Healthy disk so readiness results don't depend on the dev machine.
fn roomy_disk() -> pair_api::state::DiskProbe {
    Arc::new(|_| {
        Ok(DiskStats {
            total_bytes: 100 * BYTES_PER_GIB,
            free_bytes: 80 * BYTES_PER_GIB,
        })
    })
}

fn state_for(db: &TestDb) -> AppState {
    AppState::new(db.pool.clone(), TOKEN)
        .with_migrations_dir(unique_dir())
        .with_disk_probe(roomy_disk())
}

#[tokio::test]
async fn unauthenticated_request_rejected() {
    let app = app_with(AppState::new(dead_pool(), TOKEN));
    let no_auth = Request::builder()
        .uri("/readyz")
        .body(Body::empty())
        .expect("req");
    let (resp, body) = send(&app, no_auth).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthenticated");
    assert!(resp.headers().contains_key("x-trace-id"));

    let wrong = Request::builder()
        .uri("/v1/whoami")
        .header("authorization", "Bearer wrong-token-wrong-token")
        .header("x-actor", "tester")
        .body(Body::empty())
        .expect("req");
    assert_eq!(send(&app, wrong).await.0.status(), StatusCode::UNAUTHORIZED);

    let no_actor = Request::builder()
        .uri("/v1/whoami")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .expect("req");
    assert_eq!(
        send(&app, no_actor).await.0.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn authenticated_request_ok() {
    let db = TestDb::create().await;
    let app = app_with(state_for(&db));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ready");

    let (resp, body) = send(&app, authed("/v1/whoami")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body["data"]["actor"], "tester");
    db.drop_db().await;
}

#[tokio::test]
async fn trace_id_propagated() {
    let app = app_with(AppState::new(dead_pool(), TOKEN));
    let mut req = authed("/v1/whoami");
    req.headers_mut()
        .insert("x-trace-id", "trace-abc_123".parse().expect("hv"));
    let (resp, body) = send(&app, req).await;
    assert_eq!(resp.headers()["x-trace-id"], "trace-abc_123");
    assert_eq!(body["data"]["trace_id"], "trace-abc_123");

    // Absent or malformed inbound id: a fresh one is generated.
    let (resp, _) = send(&app, authed("/v1/whoami")).await;
    let generated = resp.headers()["x-trace-id"]
        .to_str()
        .expect("ascii")
        .to_owned();
    assert!(uuid::Uuid::parse_str(&generated).is_ok());
    let mut bad = authed("/v1/whoami");
    bad.headers_mut()
        .insert("x-trace-id", "bad id!".parse().expect("hv"));
    let (resp, _) = send(&app, bad).await;
    assert_ne!(resp.headers()["x-trace-id"], "bad id!");
}

#[tokio::test]
async fn readyz_fails_when_db_down() {
    let app = app_with(AppState::new(dead_pool(), TOKEN).with_disk_probe(roomy_disk()));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "not_ready");
}

#[tokio::test]
async fn healthz_needs_no_auth() {
    let app = app_with(AppState::new(dead_pool(), TOKEN));
    let req = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .expect("req");
    let (resp, body) = send(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(resp.headers().contains_key("x-trace-id"));
}

fn check<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    body["checks"]
        .as_array()
        .and_then(|cs| cs.iter().find(|c| c["name"] == name))
        .unwrap_or(&serde_json::Value::Null)
}

#[tokio::test]
async fn readyz_reports_migration_failure_and_pending() {
    let db = TestDb::create().await;
    let dir = unique_dir();
    std::fs::write(dir.join("0001_init.sql"), "CREATE TABLE widgets (id int);").expect("write");
    let app = app_with(state_for(&db).with_migrations_dir(&dir));

    // Pending: on disk but not applied.
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(check(&body, "migrations")["detail"]
        .as_str()
        .unwrap_or("")
        .contains("pending"));

    // Applied: ready.
    sqlx::migrate::Migrator::new(dir.as_path())
        .await
        .expect("migrator")
        .run(&db.pool)
        .await
        .expect("migrate");
    let (resp, _) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Dirty: a recorded failed migration.
    sqlx::query("UPDATE _sqlx_migrations SET success = false")
        .execute(&db.pool)
        .await
        .expect("mark dirty");
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let detail = check(&body, "migrations")["detail"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    assert!(detail.contains("failed"), "{detail}");
    db.drop_db().await;
}

#[tokio::test]
async fn readyz_queue_backlog_gauge_and_thresholds() {
    let db = TestDb::create().await;
    sqlx::query(
        "CREATE TABLE jobs (state text NOT NULL, created_at timestamptz NOT NULL DEFAULT now())",
    )
    .execute(&db.pool)
    .await
    .expect("jobs table");
    let app = app_with(state_for(&db));

    sqlx::query("INSERT INTO jobs (state) SELECT 'queued' FROM generate_series(1, 60)")
        .execute(&db.pool)
        .await
        .expect("seed");
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "warn must not fail readiness"
    );
    assert_eq!(body["queue_backlog_depth"], 60);
    assert_eq!(check(&body, "queue_backlog")["level"], "warn");

    sqlx::query("INSERT INTO jobs (state) SELECT 'queued' FROM generate_series(1, 200)")
        .execute(&db.pool)
        .await
        .expect("seed more");
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(check(&body, "queue_backlog")["level"], "critical");
    db.drop_db().await;
}

#[tokio::test]
async fn readyz_fails_when_disk_critical() {
    let db = TestDb::create().await;
    let full: pair_api::state::DiskProbe = Arc::new(|_| {
        Ok(DiskStats {
            total_bytes: 100 * BYTES_PER_GIB,
            free_bytes: BYTES_PER_GIB,
        })
    });
    let app = app_with(state_for(&db).with_disk_probe(full));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(check(&body, "disk")["level"], "critical");
    db.drop_db().await;
}

struct AllDown;

#[async_trait::async_trait]
impl ProviderHealth for AllDown {
    async fn statuses(&self) -> Vec<ProviderStatus> {
        vec![ProviderStatus {
            provider: "anthropic".into(),
            available: false,
            detail: "timeout".into(),
        }]
    }
}

#[tokio::test]
async fn readyz_provider_outage_degrades_but_stays_ready() {
    let db = TestDb::create().await;
    let app = app_with(state_for(&db).with_providers(Arc::new(AllDown)));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(check(&body, "providers")["level"], "warn");
    db.drop_db().await;
}
