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

const WORKFLOW_RUNS_SQL: &str = include_str!("../../../../migrations/030_jobs.sql");

async fn seed_runs(db: &TestDb, count: i32, state: &str, age: &str) {
    sqlx::query(
        "INSERT INTO workflow_runs (id, idempotency_key, kind, input, input_hash, run_class, \
         state, deadline_at, created_at) \
         SELECT gen_random_uuid(), gen_random_uuid(), 'k', '{}'::jsonb, 'h', 'interactive', \
         $1, now() + interval '1 hour', now() - $2::interval \
         FROM generate_series(1, $3)",
    )
    .bind(state)
    .bind(age)
    .bind(count)
    .execute(&db.pool)
    .await
    .expect("seed runs");
}

#[tokio::test]
async fn readyz_queue_backlog_reads_workflow_runs() {
    let db = TestDb::create().await;
    sqlx::raw_sql(WORKFLOW_RUNS_SQL)
        .execute(&db.pool)
        .await
        .expect("apply 030_jobs.sql");
    let app = app_with(state_for(&db));

    // Non-queued runs never count.
    seed_runs(&db, 500, "succeeded", "2 hours").await;
    let (_, body) = send(&app, authed("/readyz")).await;
    assert_eq!(body["queue_backlog_depth"], 0);
    assert_eq!(check(&body, "queue_backlog")["level"], "ok");

    // Depth warn.
    seed_runs(&db, 60, "queued", "1 second").await;
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "warn must not fail readiness"
    );
    assert_eq!(body["queue_backlog_depth"], 60);
    assert_eq!(check(&body, "queue_backlog")["level"], "warn");

    // Depth critical.
    seed_runs(&db, 200, "queued", "1 second").await;
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(check(&body, "queue_backlog")["level"], "critical");
    db.drop_db().await;
}

#[tokio::test]
async fn readyz_queue_backlog_age_thresholds() {
    let db = TestDb::create().await;
    sqlx::raw_sql(WORKFLOW_RUNS_SQL)
        .execute(&db.pool)
        .await
        .expect("apply 030_jobs.sql");
    let app = app_with(state_for(&db));

    seed_runs(&db, 1, "queued", "10 minutes").await;
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(check(&body, "queue_backlog")["level"], "warn");

    seed_runs(&db, 1, "queued", "40 minutes").await;
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

const REGISTRY_YAML: &str = r#"
models:
  - id: m-a
    provider: anthropic
    endpoint: https://api.anthropic.com
    context_tokens: 200000
    max_output_tokens: 8192
    data_policy: p
    allowed_data_classes: [public]
  - id: m-o
    provider: ollama_cloud
    endpoint: https://ollama.com
    context_tokens: 128000
    max_output_tokens: 8192
    data_policy: p
    allowed_data_classes: [public]
"#;

#[tokio::test]
async fn readyz_unhealthy_registry_entry_degrades_but_stays_ready() {
    use pair_models::provider::{Health, ProviderRegistry};
    let db = TestDb::create().await;
    let healthy = ProviderRegistry::from_yaml_str(REGISTRY_YAML).expect("registry");
    let app = app_with(state_for(&db).with_providers(Arc::new(healthy.clone())));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(check(&body, "providers")["level"], "ok");
    assert_eq!(body["providers"].as_array().map(Vec::len), Some(2));

    let degraded = healthy.with_health("m-a", Health::Degraded);
    let app = app_with(state_for(&db).with_providers(Arc::new(degraded)));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "provider outage must not 503"
    );
    assert_eq!(check(&body, "providers")["level"], "warn");

    let disabled = healthy
        .with_health("m-a", Health::Disabled)
        .with_health("m-o", Health::Disabled);
    let app = app_with(state_for(&db).with_providers(Arc::new(disabled)));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(check(&body, "providers")["level"], "warn");
    db.drop_db().await;
}
