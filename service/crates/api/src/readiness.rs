//! `/readyz`: database, migrations, queue backlog, disk, provider availability.
//! Any `Critical` check makes the service not ready (HTTP 503).

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use pair_telemetry::health::{check_disk_free, evaluate_backlog, CheckResult, HealthLevel};
use serde::Serialize;
use sqlx::migrate::Migrator;
use sqlx::PgPool;

use crate::providers::ProviderStatus;
use crate::state::AppState;

const DB_TIMEOUT: Duration = Duration::from_secs(3);
const MIGRATIONS_TABLE: &str = "_sqlx_migrations";
/// Durable run table (migrations/030_jobs.sql); `queued` rows form the backlog.
const RUNS_TABLE: &str = "workflow_runs";
const RUNS_QUEUED_STATE: &str = "queued";

#[derive(Debug, Serialize)]
pub struct ReadyReport {
    pub status: &'static str,
    pub checks: Vec<CheckResult>,
    pub queue_backlog_depth: Option<u64>,
    pub providers: Vec<ProviderStatus>,
}

impl ReadyReport {
    pub fn is_ready(&self) -> bool {
        !self.checks.iter().any(|c| c.level == HealthLevel::Critical)
    }
}

pub async fn build_report(state: &AppState) -> ReadyReport {
    let mut checks = Vec::new();
    let mut depth = None;

    let db = check_db(state.pool()).await;
    let db_up = db.level == HealthLevel::Ok;
    checks.push(db);
    if db_up {
        checks.push(check_migrations(state.pool(), &state.migrations_dir).await);
        let (backlog, d) = check_backlog(state.pool()).await;
        checks.push(backlog);
        depth = d;
    } else {
        checks.push(CheckResult::new(
            "migrations",
            HealthLevel::Critical,
            "cannot verify: database unreachable",
        ));
    }
    checks.push(check_disk(state).await);

    let providers = state.providers.statuses().await;
    checks.push(provider_check(&providers));

    let mut report = ReadyReport {
        status: "ready",
        checks,
        queue_backlog_depth: depth,
        providers,
    };
    if !report.is_ready() {
        report.status = "not_ready";
    }
    report
}

async fn check_db(pool: &PgPool) -> CheckResult {
    let probe = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool);
    match tokio::time::timeout(DB_TIMEOUT, probe).await {
        Ok(Ok(_)) => CheckResult::new("database", HealthLevel::Ok, "reachable"),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "readyz: database check failed");
            CheckResult::new("database", HealthLevel::Critical, "unreachable")
        }
        Err(_) => CheckResult::new("database", HealthLevel::Critical, "timed out"),
    }
}

/// Versions of forward migrations present on disk. Missing directory means none.
async fn expected_versions(dir: &Path) -> Result<HashSet<i64>, String> {
    if !dir.is_dir() {
        return Ok(HashSet::new());
    }
    let migrator = Migrator::new(dir).await.map_err(|e| e.to_string())?;
    Ok(migrator
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .map(|m| m.version)
        .collect())
}

async fn check_migrations(pool: &PgPool, dir: &Path) -> CheckResult {
    let expected = match expected_versions(dir).await {
        Ok(v) => v,
        Err(e) => {
            return CheckResult::new(
                "migrations",
                HealthLevel::Critical,
                format!("cannot read migrations: {e}"),
            )
        }
    };
    let rows = applied_migrations(pool).await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "readyz: migrations query failed");
            return CheckResult::new("migrations", HealthLevel::Critical, "cannot query state");
        }
    };
    if let Some((version, _)) = rows.iter().find(|(_, success)| !success) {
        return CheckResult::new(
            "migrations",
            HealthLevel::Critical,
            format!("migration {version} failed (dirty); see migration-failure runbook"),
        );
    }
    let applied: HashSet<i64> = rows.iter().map(|(v, _)| *v).collect();
    let mut pending: Vec<i64> = expected.difference(&applied).copied().collect();
    pending.sort_unstable();
    if pending.is_empty() {
        CheckResult::new(
            "migrations",
            HealthLevel::Ok,
            format!("{} applied", applied.len()),
        )
    } else {
        CheckResult::new(
            "migrations",
            HealthLevel::Critical,
            format!("pending migrations: {pending:?}"),
        )
    }
}

async fn applied_migrations(pool: &PgPool) -> Result<Vec<(i64, bool)>, sqlx::Error> {
    if !table_exists(pool, MIGRATIONS_TABLE).await? {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, (i64, bool)>(&format!(
        "SELECT version, success FROM {MIGRATIONS_TABLE} ORDER BY version"
    ))
    .fetch_all(pool)
    .await
}

async fn table_exists(pool: &PgPool, table: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
        .bind(table)
        .fetch_one(pool)
        .await
}

async fn check_backlog(pool: &PgPool) -> (CheckResult, Option<u64>) {
    match read_backlog(pool).await {
        Ok(Some((depth, age))) => (evaluate_backlog(depth, age), Some(depth)),
        Ok(None) => (
            CheckResult::new(
                "queue_backlog",
                HealthLevel::Ok,
                "workflow_runs table not present yet",
            ),
            Some(0),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "readyz: backlog query failed");
            (
                CheckResult::new("queue_backlog", HealthLevel::Warn, "unable to read backlog"),
                None,
            )
        }
    }
}

/// `(queued depth, age in seconds of oldest queued run)`; `None` if no workflow_runs table.
async fn read_backlog(pool: &PgPool) -> Result<Option<(u64, u64)>, sqlx::Error> {
    if !table_exists(pool, RUNS_TABLE).await? {
        return Ok(None);
    }
    let (depth, age) = sqlx::query_as::<_, (i64, i64)>(&format!(
        "SELECT count(*)::bigint, \
         COALESCE(EXTRACT(EPOCH FROM (now() - min(created_at))), 0)::bigint \
         FROM {RUNS_TABLE} WHERE state = $1"
    ))
    .bind(RUNS_QUEUED_STATE)
    .fetch_one(pool)
    .await?;
    Ok(Some((depth.max(0) as u64, age.max(0) as u64)))
}

async fn check_disk(state: &AppState) -> CheckResult {
    let probe = state.disk_probe.clone();
    let dir = state.data_dir.clone();
    match tokio::task::spawn_blocking(move || check_disk_free(&dir, |p| probe(p))).await {
        Ok(r) => r,
        Err(e) => CheckResult::new("disk", HealthLevel::Warn, format!("probe task failed: {e}")),
    }
}

/// Provider outage degrades (Warn) but never fails readiness: PAIR must stay up
/// to serve memory, budget and approvals while a provider is down.
fn provider_check(providers: &[ProviderStatus]) -> CheckResult {
    if providers.is_empty() {
        return CheckResult::new(
            "providers",
            HealthLevel::Ok,
            "no providers registered (stub)",
        );
    }
    let down: Vec<&str> = providers
        .iter()
        .filter(|p| !p.available)
        .map(|p| p.provider.as_str())
        .collect();
    if down.is_empty() {
        CheckResult::new("providers", HealthLevel::Ok, "all available")
    } else {
        CheckResult::new(
            "providers",
            HealthLevel::Warn,
            format!("unavailable: {down:?}"),
        )
    }
}
