//! Readiness strictness, response sanitization, actor charset and request limits.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderValue, StatusCode};
use common::{authed, dead_pool, send, unique_dir, TestDb, TOKEN};
use pair_api::limits::Limits;
use pair_api::state::{AppState, DiskProbe};
use pair_telemetry::health::{DiskStats, BYTES_PER_GIB};

fn roomy_disk() -> DiskProbe {
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

fn check<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    body["checks"]
        .as_array()
        .and_then(|cs| cs.iter().find(|c| c["name"] == name))
        .unwrap_or(&serde_json::Value::Null)
}

#[tokio::test]
async fn readyz_warns_on_stub_providers() {
    let db = TestDb::create().await;
    let app = pair_api::router(state_for(&db));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "warn must not fail readiness"
    );
    assert_eq!(check(&body, "providers")["level"], "warn");
    db.drop_db().await;
}

#[tokio::test]
async fn readyz_fails_when_no_migrations_applied() {
    let db = TestDb::create().await;
    // Required, directory present but empty (a mount that came up empty).
    let app = pair_api::router(state_for(&db).with_migrations_required(true));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(check(&body, "migrations")["level"], "critical");

    // Required, directory missing entirely.
    let missing =
        std::env::temp_dir().join(format!("pair_missing_{}", uuid::Uuid::new_v4().simple()));
    let app = pair_api::router(
        state_for(&db)
            .with_migrations_dir(&missing)
            .with_migrations_required(true),
    );
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(check(&body, "migrations")["level"], "critical");

    // Required, migrations on disk and applied: ready.
    let dir = unique_dir();
    std::fs::write(dir.join("0001_init.sql"), "CREATE TABLE widgets (id int);").expect("write");
    sqlx::migrate::Migrator::new(dir.as_path())
        .await
        .expect("migrator")
        .run(&db.pool)
        .await
        .expect("migrate");
    let app = pair_api::router(
        state_for(&db)
            .with_migrations_dir(&dir)
            .with_migrations_required(true),
    );
    let (resp, _) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    db.drop_db().await;
}

#[tokio::test]
async fn readyz_error_details_do_not_leak_paths_or_os_errors() {
    use std::os::unix::fs::PermissionsExt;
    let db = TestDb::create().await;
    let dir = unique_dir();
    let sql = dir.join("0001_init.sql");
    std::fs::write(&sql, "SELECT 1;").expect("write");
    std::fs::set_permissions(&sql, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let failing_disk: DiskProbe = Arc::new(|p| {
        Err(std::io::Error::other(format!(
            "No such file or directory: {}",
            p.display()
        )))
    });
    let state = state_for(&db)
        .with_migrations_dir(&dir)
        .with_disk_probe(failing_disk)
        .with_data_dir("/very/secret/data/dir");
    let (_, body) = send(&pair_api::router(state), authed("/readyz")).await;
    let text = body.to_string();
    for needle in [
        dir.to_string_lossy().as_ref(),
        "/very/secret",
        "Permission denied",
        "No such file",
        "os error",
    ] {
        assert!(!text.contains(needle), "leaked {needle}: {text}");
    }
    assert_eq!(check(&body, "disk")["detail"], "statfs failed");
    std::fs::set_permissions(&sql, std::fs::Permissions::from_mode(0o600)).expect("chmod back");
    db.drop_db().await;
}

#[tokio::test]
async fn actor_header_restricted_to_safe_charset() {
    let app = pair_api::router(AppState::new(dead_pool(), TOKEN));
    let with_actor = |actor: &str| {
        let mut req = authed("/v1/whoami");
        req.headers_mut()
            .insert("x-actor", actor.parse().expect("header value"));
        req
    };
    let (resp, body) = send(&app, with_actor("svc:open-claw_1@host.local")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body["data"]["actor"], "svc:open-claw_1@host.local");
    for bad in [
        "has space",
        "semi;colon",
        "slash/x",
        "<script>",
        &"a".repeat(65),
    ] {
        let (resp, _) = send(&app, with_actor(bad)).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{bad}");
    }
    let mut unicode = authed("/v1/whoami");
    unicode.headers_mut().insert(
        "x-actor",
        HeaderValue::from_bytes("caf\u{e9}".as_bytes()).expect("opaque bytes"),
    );
    assert_eq!(
        send(&app, unicode).await.0.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn request_timeout_returns_504_with_trace_id() {
    // A dead pool makes /readyz take ~500ms (pool acquire timeout); the 50ms limit cuts it off.
    let limits = Limits {
        request_timeout: Duration::from_millis(50),
        max_in_flight: 8,
    };
    let app = pair_api::router(AppState::new(dead_pool(), TOKEN).with_limits(limits));
    let (resp, body) = send(&app, authed("/readyz")).await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["error"]["code"], "limit_exceeded");
    assert!(resp.headers().contains_key("x-trace-id"));
}
