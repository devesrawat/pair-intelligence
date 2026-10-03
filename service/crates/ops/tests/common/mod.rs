//! Scratch database with the full migration chain applied. Never touches the shared `pair` database.
#![allow(dead_code)]
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_DATABASE_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";

fn base_options() -> PgConnectOptions {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_owned())
        .parse()
        .expect("DATABASE_URL parses")
}

pub struct TestDb {
    pub pool: PgPool,
    name: String,
}

impl TestDb {
    /// Creates `pair_t_ops_<uuid>` and applies every migration in order.
    pub async fn create() -> Self {
        let name = format!("pair_t_ops_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnection::connect_with(&base_options())
            .await
            .expect("admin connect");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .expect("create scratch db");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(base_options().database(&name))
            .await
            .expect("scratch connect");
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        Migrator::new(dir)
            .await
            .expect("load migrations")
            .run(&pool)
            .await
            .expect("apply migrations");
        Self { pool, name }
    }

    pub async fn drop_db(self) {
        self.pool.close().await;
        let mut admin = PgConnection::connect_with(&base_options())
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

pub fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pair_ops_{tag}_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub async fn insert_source(pool: &PgPool, age_days: i64, pinned: bool, uri: &str) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sources (id, kind, external_id, revision, content_hash, captured_at, data_class, trust, uri, pinned) \
         VALUES ($1, 'import', $2, 1, 'h', now() - make_interval(days => $3::int), 'personal', 'owner', $4, $5)",
    )
    .bind(id)
    .bind(id.to_string())
    .bind(i32::try_from(age_days).expect("days fit"))
    .bind(uri)
    .bind(pinned)
    .execute(pool)
    .await
    .expect("insert source");
    id
}

/// Inserts a memory together with its evidence row in one transaction (the evidence trigger is deferred).
pub async fn insert_memory(
    pool: &PgPool,
    kind: &str,
    content: &str,
    source: uuid::Uuid,
    span: &str,
    age_days: i64,
) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO memories (id, kind, status, content, normalized_content, valid_from, observed_at, confidence, accepted_by, created_at) \
         VALUES ($1, $2, 'accepted', $3, lower($3), now() - make_interval(days => $4::int), now() - make_interval(days => $4::int), 'observed', 'owner', now() - make_interval(days => $4::int))",
    )
    .bind(id)
    .bind(kind)
    .bind(content)
    .bind(i32::try_from(age_days).expect("days fit"))
    .execute(&mut *tx)
    .await
    .expect("insert memory");
    sqlx::query(
        "INSERT INTO memory_evidence (memory_id, source_id, span, extraction_version) VALUES ($1, $2, $3, 'v1')",
    )
    .bind(id)
    .bind(source)
    .bind(span)
    .execute(&mut *tx)
    .await
    .expect("insert evidence");
    tx.commit().await.expect("commit");
    id
}

pub async fn insert_model_call(
    pool: &PgPool,
    age_days: i64,
    request: Option<&str>,
    response: Option<&str>,
    reservation: Option<uuid::Uuid>,
) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO model_calls (id, task_id, trace_id, reservation_id, provider, requested_model, cost_state, route_reason, latency_ms, status, created_at, request_payload, response_payload) \
         VALUES ($1, $2, $2, $3, 'anthropic', 'm', 'priced', 'test', 5, 'ok', now() - make_interval(days => $4::int), $5, $6)",
    )
    .bind(id)
    .bind(uuid::Uuid::new_v4())
    .bind(reservation)
    .bind(i32::try_from(age_days).expect("days fit"))
    .bind(request)
    .bind(response)
    .execute(pool)
    .await
    .expect("insert model call");
    id
}

pub async fn insert_reservation(pool: &PgPool, state: &str) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let resolved = if state == "held" { "NULL" } else { "now()" };
    sqlx::query(&format!(
        "INSERT INTO budget_reservations (id, task_id, category, task_kind, price_version, period_day, period_month, reserved_micros, counted_micros, state, created_at, resolved_at) \
         VALUES ($1, $2, 'metered', 'default', 'p1', current_date, date_trunc('month', current_date)::date, 1000, 1000, $3, now() - interval '60 days', {resolved})"
    ))
    .bind(id)
    .bind(uuid::Uuid::new_v4())
    .bind(state)
    .execute(pool)
    .await
    .expect("insert reservation");
    id
}
