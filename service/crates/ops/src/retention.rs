//! Retention purge (spec section 11).
//!
//! Defaults: detailed model/tool payloads 30 days; raw imported sources 90 days unless pinned.
//! Decisions and accepted memories are never purged by age: no `memories`, `memory_evidence` or
//! `budget_*` row is ever modified here. Purging overwrites content (NULL or a fixed marker);
//! ids, hashes, usage, cost and audit rows are kept. Work is done in bounded batches and every
//! predicate excludes already-erased rows, so repeating a purge is a no-op.
use crate::error::{OpsError, Result};
use crate::schema::{self, ColumnInfo};
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

const DEFAULT_PAYLOAD_DAYS: i64 = 30;
const DEFAULT_SOURCE_DAYS: i64 = 90;
const DEFAULT_BATCH_SIZE: i64 = 500;
const DEFAULT_MAX_BATCHES: u32 = 100;

/// Payload-like columns of `tool_executions`, erased when present. The table is owned by another
/// migration, so its shape is detected rather than assumed. The real table (migration 090) keeps
/// only an args hash and a destination reduced to `scheme://host[:port][/path]` at record time, so
/// it has none of these: `destination` is deliberately NOT listed, it is egress audit evidence.
const TOOL_PAYLOAD_COLUMNS: &[&str] = &[
    "request",
    "args",
    "arguments",
    "input",
    "payload",
    "output",
    "result",
    "response",
    "stdout",
    "stderr",
];
const TOOL_AGE_COLUMNS: &[&str] = &["created_at", "started_at", "requested_at"];

/// Targets every deployment has. If one cannot run (missing table or column, or a decoy table
/// earlier on the search_path) the purge is refused: a quiet skip would report success while old
/// payloads stay behind.
const CORE_TARGETS: &[&str] = &[
    "model_calls.payloads",
    "effect_intents.payloads",
    "sources.raw",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub payload_retention: Duration,
    pub source_retention: Duration,
    /// Rows erased per statement.
    pub batch_size: i64,
    /// Statements per target before the run stops and reports `truncated`.
    pub max_batches_per_target: u32,
    /// Count what would be erased without erasing anything.
    pub dry_run: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            payload_retention: Duration::days(DEFAULT_PAYLOAD_DAYS),
            source_retention: Duration::days(DEFAULT_SOURCE_DAYS),
            batch_size: DEFAULT_BATCH_SIZE,
            max_batches_per_target: DEFAULT_MAX_BATCHES,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetReport {
    pub target: String,
    pub erased: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PurgeReport {
    pub targets: Vec<TargetReport>,
    /// Targets skipped because the table or a required column does not exist in this database.
    pub skipped: Vec<String>,
    /// True when a target hit its batch limit while work remained; run purge again.
    pub truncated: bool,
    pub dry_run: bool,
}

impl PurgeReport {
    pub fn erased(&self, target: &str) -> u64 {
        self.targets
            .iter()
            .find(|t| t.target == target)
            .map_or(0, |t| t.erased)
    }

    pub fn total(&self) -> u64 {
        self.targets.iter().map(|t| t.erased).sum()
    }
}

#[derive(Clone, Copy)]
enum Age {
    Payload,
    Source,
}

/// A statically defined purge target. All SQL fragments are constants; times and limits are bound.
/// `pred` may use only `$1` (the cutoff); `set` may use only `$2` (now).
struct Target {
    name: &'static str,
    table: &'static str,
    required: &'static [&'static str],
    set: &'static str,
    pred: &'static str,
    age: Age,
}

const STATIC_TARGETS: &[Target] = &[
    Target {
        name: "model_calls.payloads",
        table: "model_calls",
        required: &["request_payload", "response_payload", "payload_purged_at", "reservation_id"],
        set: "request_payload = NULL, response_payload = NULL, payload_purged_at = $2",
        // Calls behind a held or unresolved reservation keep their payloads: they are the evidence
        // for a charge nobody has settled yet.
        pred: "created_at < $1 AND payload_purged_at IS NULL \
               AND (request_payload IS NOT NULL OR response_payload IS NOT NULL) \
               AND (reservation_id IS NULL OR NOT EXISTS ( \
                    SELECT 1 FROM budget_reservations r \
                    WHERE r.id = model_calls.reservation_id AND r.state <> 'settled'))",
        age: Age::Payload,
    },
    Target {
        name: "effect_intents.payloads",
        table: "effect_intents",
        required: &["run_id", "status", "payload", "result", "created_at"],
        set: "payload = 'null'::jsonb, result = NULL",
        // Only finished intents of finished runs: unresolved intents are reconciled from their payload.
        pred: "created_at < $1 AND status IN ('completed', 'not_applied') \
               AND (payload <> 'null'::jsonb OR result IS NOT NULL) \
               AND run_id IN (SELECT id FROM workflow_runs WHERE state IN ('succeeded', 'failed', 'cancelled'))",
        age: Age::Payload,
    },
    Target {
        name: "sources.raw",
        table: "sources",
        required: &["uri", "pinned", "purged_at", "captured_at"],
        set: "uri = NULL, purged_at = $2",
        pred: "captured_at < $1 AND NOT pinned AND purged_at IS NULL AND uri IS NOT NULL",
        age: Age::Source,
    },
    Target {
        name: "research_sources.text",
        table: "research_sources",
        required: &["text", "pinned", "fetched_at"],
        // `text` is NOT NULL (an available source must carry text), so the erased form is empty.
        set: "text = ''",
        pred: "fetched_at < $1 AND NOT pinned AND text IS NOT NULL AND text <> ''",
        age: Age::Source,
    },
    Target {
        name: "research_claim_evidence.span",
        table: "research_claim_evidence",
        required: &["id", "span", "source_id"],
        // `span` is NOT NULL and quotes the source verbatim, so it goes with the source text.
        set: "span = '[purged]'",
        pred: "span <> '[purged]' AND source_id IN ( \
               SELECT s.id FROM research_sources s WHERE s.fetched_at < $1 AND NOT s.pinned)",
        age: Age::Source,
    },
    Target {
        name: "research_reports.markdown",
        table: "research_reports",
        required: &["run_id", "markdown"],
        // The rendered report quotes the sources of its run; once one is purged it must go too.
        set: "markdown = ''",
        pred: "markdown <> '' AND run_id IN ( \
               SELECT s.run_id FROM research_sources s WHERE s.fetched_at < $1 AND NOT s.pinned)",
        age: Age::Source,
    },
    Target {
        name: "integration_sources.content",
        table: "integration_sources",
        required: &["content", "pinned", "updated_at"],
        set: "content = NULL",
        pred: "updated_at < $1 AND NOT pinned AND content IS NOT NULL",
        age: Age::Source,
    },
    Target {
        name: "memory_candidate_evidence.spans",
        table: "memory_candidate_evidence",
        required: &["id", "span", "candidate_id", "source_id"],
        // Unique index covers (candidate, source, span), so each erased span stays distinct.
        set: "span = '[purged:' || id::text || ']'",
        // Only evidence of REJECTED candidates; accepted memories keep their evidence.
        pred: "span IS NOT NULL AND span <> '[purged:' || id::text || ']' \
               AND candidate_id IN (SELECT c.id FROM memory_candidates c WHERE c.state = 'rejected') \
               AND source_id IN (SELECT s.id FROM sources s WHERE s.captured_at < $1 AND NOT s.pinned)",
        age: Age::Source,
    },
];

/// Statement pair for one target: the erasing UPDATE and the matching COUNT.
struct Statements {
    update: String,
    count: String,
}

fn statements(table: &str, set: &str, pred: &str) -> Statements {
    Statements {
        update: format!(
            // `$2 IS NOT NULL` pins the type of $2 even when `set` does not use it.
            "UPDATE {table} SET {set} WHERE $2::timestamptz IS NOT NULL AND ctid IN \
             (SELECT ctid FROM {table} WHERE {pred} LIMIT $3 FOR UPDATE SKIP LOCKED)"
        ),
        count: format!("SELECT count(*) FROM {table} WHERE {pred}"),
    }
}

/// Erases aged content as of `now` under `policy`. See the module docs for what is and is not touched.
pub async fn purge(
    pool: &PgPool,
    now: DateTime<Utc>,
    policy: &RetentionPolicy,
) -> Result<PurgeReport> {
    if policy.batch_size < 1 || policy.max_batches_per_target < 1 {
        return Err(OpsError::InvalidArgument(
            "batch_size and max_batches_per_target must be at least 1".into(),
        ));
    }
    let mut report = PurgeReport {
        dry_run: policy.dry_run,
        ..PurgeReport::default()
    };
    let mut runnable = Vec::with_capacity(STATIC_TARGETS.len());
    for target in STATIC_TARGETS {
        let cols = schema::columns(pool, target.table).await?;
        if schema::has_all(&cols, target.required) {
            runnable.push(target);
        } else {
            report.skipped.push(target.name.to_owned());
        }
    }
    let missing_core: Vec<String> = report
        .skipped
        .iter()
        .filter(|name| CORE_TARGETS.contains(&name.as_str()))
        .cloned()
        .collect();
    if !missing_core.is_empty() {
        return Err(OpsError::CoreTargetSkipped(missing_core));
    }
    for target in runnable {
        let cutoff = cutoff_for(target.age, now, policy);
        let sql = statements(target.table, target.set, target.pred);
        run_target(pool, target.name, &sql, cutoff, now, policy, &mut report).await?;
    }
    purge_tool_executions(pool, now, policy, &mut report).await?;
    tracing::info!(
        total = report.total(),
        truncated = report.truncated,
        dry_run = report.dry_run,
        "retention purge finished"
    );
    Ok(report)
}

fn cutoff_for(age: Age, now: DateTime<Utc>, policy: &RetentionPolicy) -> DateTime<Utc> {
    match age {
        Age::Payload => now - policy.payload_retention,
        Age::Source => now - policy.source_retention,
    }
}

async fn purge_tool_executions(
    pool: &PgPool,
    now: DateTime<Utc>,
    policy: &RetentionPolicy,
    report: &mut PurgeReport,
) -> Result<()> {
    const NAME: &str = "tool_executions.payloads";
    let cols = schema::columns(pool, "tool_executions").await?;
    let Some((set, pred)) = tool_clauses(&cols) else {
        report.skipped.push(NAME.to_owned());
        return Ok(());
    };
    let sql = statements("tool_executions", &set, &pred);
    let cutoff = now - policy.payload_retention;
    run_target(pool, NAME, &sql, cutoff, now, policy, report).await
}

/// Builds SET/WHERE for `tool_executions` from the columns that exist. Identifiers come only from
/// the constant lists above (matched against the catalog), never from data.
fn tool_clauses(cols: &[ColumnInfo]) -> Option<(String, String)> {
    let age = TOOL_AGE_COLUMNS
        .iter()
        .find(|a| cols.iter().any(|c| c.name == **a))?;
    let mut sets = Vec::new();
    let mut unerased = Vec::new();
    for col in cols
        .iter()
        .filter(|c| TOOL_PAYLOAD_COLUMNS.contains(&c.name.as_str()))
    {
        let name = &col.name;
        let (erased_value, not_erased) = match (col.nullable, col.data_type.as_str()) {
            (true, _) => ("NULL", format!("\"{name}\" IS NOT NULL")),
            (false, "jsonb") => ("'null'::jsonb", format!("\"{name}\" <> 'null'::jsonb")),
            (false, "json") => ("'null'::json", format!("\"{name}\"::text <> 'null'")),
            (false, "text" | "character varying") => ("''", format!("\"{name}\" <> ''")),
            _ => continue,
        };
        sets.push(format!("\"{name}\" = {erased_value}"));
        unerased.push(not_erased);
    }
    if sets.is_empty() {
        return None;
    }
    // A row still `started` is evidence of an unfinished action: never erased.
    let finished = if cols.iter().any(|c| c.name == "outcome") {
        " AND \"outcome\" IS DISTINCT FROM 'started'"
    } else {
        ""
    };
    let pred = format!("\"{age}\" < $1 AND ({}){finished}", unerased.join(" OR "));
    Some((sets.join(", "), pred))
}

async fn run_target(
    pool: &PgPool,
    name: &str,
    sql: &Statements,
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
    policy: &RetentionPolicy,
    report: &mut PurgeReport,
) -> Result<()> {
    let erased = if policy.dry_run {
        u64::try_from(remaining(pool, sql, cutoff).await?).unwrap_or(0)
    } else {
        let erased = erase_in_batches(pool, sql, cutoff, now, policy).await?;
        // `FOR UPDATE SKIP LOCKED` can return fewer rows than the batch while locked rows remain,
        // and a run that exactly covers the work is not truncated: ask what is actually left.
        if remaining(pool, sql, cutoff).await? > 0 {
            report.truncated = true;
        }
        erased
    };
    tracing::info!(
        target = name,
        erased,
        dry_run = policy.dry_run,
        "purge target done"
    );
    report.targets.push(TargetReport {
        target: name.to_owned(),
        erased,
    });
    Ok(())
}

async fn remaining(pool: &PgPool, sql: &Statements, cutoff: DateTime<Utc>) -> Result<i64> {
    Ok(sqlx::query_scalar(&sql.count)
        .bind(cutoff)
        .fetch_one(pool)
        .await?)
}

async fn erase_in_batches(
    pool: &PgPool,
    sql: &Statements,
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
    policy: &RetentionPolicy,
) -> Result<u64> {
    let batch = u64::try_from(policy.batch_size).unwrap_or(1);
    let mut erased: u64 = 0;
    for _ in 0..policy.max_batches_per_target {
        let done = sqlx::query(&sql.update)
            .bind(cutoff)
            .bind(now)
            .bind(policy.batch_size)
            .execute(pool)
            .await?
            .rows_affected();
        erased += done;
        if done < batch {
            break;
        }
    }
    Ok(erased)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
        }
    }

    #[test]
    fn test_tool_clauses_handles_nullable_and_not_null_columns() {
        let cols = [
            col("created_at", "timestamp with time zone", false),
            col("args", "jsonb", false),
            col("stdout", "text", true),
            col("tool_name", "text", false),
        ];
        let (set, pred) = tool_clauses(&cols).expect("clauses");
        assert!(set.contains("\"args\" = 'null'::jsonb") && set.contains("\"stdout\" = NULL"));
        assert!(!set.contains("tool_name"));
        assert!(pred.starts_with("\"created_at\" < $1"));
    }

    #[test]
    fn test_tool_clauses_none_without_age_or_payload_columns() {
        assert!(tool_clauses(&[col("args", "jsonb", true)]).is_none());
        assert!(tool_clauses(&[col("created_at", "timestamptz", false)]).is_none());
    }
}
