//! Test helpers: per-test scratch database and request plumbing.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response};
use axum::Router;
use http_body_util::BodyExt;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use tower::ServiceExt;

pub const TOKEN: &str = "test-service-token-0123456789";
const DEFAULT_DATABASE_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";

pub fn base_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_owned())
}

/// Scratch database `pair_t_api_<uuid>`; call `drop_db` at the end of the test.
pub struct TestDb {
    pub pool: PgPool,
    name: String,
}

impl TestDb {
    pub async fn create() -> Self {
        let opts: PgConnectOptions = base_url().parse().expect("DATABASE_URL parses");
        let name = format!("pair_t_api_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnection::connect_with(&opts)
            .await
            .expect("admin connect");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .expect("create scratch db");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(3))
            .connect_with(opts.database(&name))
            .await
            .expect("scratch connect");
        Self { pool, name }
    }

    pub async fn drop_db(self) {
        self.pool.close().await;
        let opts: PgConnectOptions = base_url().parse().expect("DATABASE_URL parses");
        let mut admin = PgConnection::connect_with(&opts)
            .await
            .expect("admin connect");
        sqlx::query(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ))
        .execute(&mut admin)
        .await
        .expect("drop scratch db");
    }
}

/// Pool pointing at a closed port: every query fails (simulates DB down).
pub fn dead_pool() -> PgPool {
    let opts = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(1)
        .username("x")
        .database("x");
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy_with(opts)
}

pub fn unique_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pair_api_mig_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn authed(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-actor", "tester")
        .body(Body::empty())
        .expect("request builds")
}

pub async fn send(app: &Router, req: Request<Body>) -> (Response<Body>, serde_json::Value) {
    let resp = app.clone().oneshot(req).await.expect("infallible service");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.expect("body collects").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (Response::from_parts(parts, Body::empty()), json)
}
