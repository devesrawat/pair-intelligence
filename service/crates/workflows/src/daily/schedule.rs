//! Scheduled routines: parsed from `config/schedule.yaml`, run as bounded jobs, and
//! enabled only after the manual versions have passed their checks.
use super::brief::{build_brief, Brief};
use super::review::{build_review, Review};
use super::store::db_err;
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use pair_core::error::{ErrorCode, PairError, Result};
use serde::Deserialize;
use sqlx::{PgPool, Row};
use std::time::Duration as StdDuration;

const LOCAL_TIME_FORMAT: &str = "%H:%M";
/// Days scanned forward when computing the next run (covers any DST gap/overlap).
const NEXT_RUN_SEARCH_DAYS: i64 = 3;
/// Step used to move past a nonexistent local time (spring-forward gap).
const GAP_STEP_MINUTES: i64 = 30;
/// Maximum total shift past a nonexistent local time.
const GAP_MAX_SHIFT_MINUTES: i64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutineKind {
    Briefing,
    Review,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Routine {
    pub name: String,
    pub kind: RoutineKind,
    pub local_time: NaiveTime,
    pub timezone: Tz,
    pub max_runtime: StdDuration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScheduledOutput {
    Brief(Brief),
    Review(Review),
}

fn invalid(msg: impl Into<String>) -> PairError {
    PairError::new(ErrorCode::InvalidInput, msg)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleFile {
    routines: Vec<RawRoutine>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoutine {
    name: String,
    kind: RoutineKind,
    local_time: String,
    timezone: String,
    max_runtime_secs: u64,
}

impl RawRoutine {
    fn into_routine(self) -> Result<Routine> {
        Ok(Routine {
            local_time: NaiveTime::parse_from_str(&self.local_time, LOCAL_TIME_FORMAT)
                .map_err(|e| invalid(format!("bad local_time {}: {e}", self.local_time)))?,
            timezone: self
                .timezone
                .parse::<Tz>()
                .map_err(|e| invalid(format!("bad timezone {}: {e}", self.timezone)))?,
            max_runtime: StdDuration::from_secs(self.max_runtime_secs),
            kind: self.kind,
            name: self.name,
        })
    }
}

/// Parse `config/schedule.yaml`. Any structural or value error rejects the whole file.
pub fn parse_schedule(text: &str) -> Result<Vec<Routine>> {
    let file: ScheduleFile =
        serde_yaml_ng::from_str(text).map_err(|e| invalid(format!("bad schedule: {e}")))?;
    if file.routines.is_empty() {
        return Err(invalid("schedule defines no routines"));
    }
    file.routines
        .into_iter()
        .map(RawRoutine::into_routine)
        .collect()
}

fn resolve_local(tz: Tz, date: chrono::NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
    let mut shifted = 0;
    while shifted <= GAP_MAX_SHIFT_MINUTES {
        let local = date.and_time(time) + Duration::minutes(shifted);
        // Ambiguous (DST fall-back): take the earlier instant. Gap (spring-forward): shift forward.
        if let Some(t) = tz.from_local_datetime(&local).earliest() {
            return Some(t.with_timezone(&Utc));
        }
        shifted += GAP_STEP_MINUTES;
    }
    None
}

/// Next firing strictly after `after`, as a UTC instant, honouring the routine's zone and DST.
pub fn next_run(routine: &Routine, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let first_day = after.with_timezone(&routine.timezone).date_naive();
    (0..=NEXT_RUN_SEARCH_DAYS)
        .filter_map(|d| {
            resolve_local(
                routine.timezone,
                first_day + Duration::days(d),
                routine.local_time,
            )
        })
        .find(|t| *t > after)
        .ok_or_else(|| {
            PairError::new(
                ErrorCode::Internal,
                format!("cannot compute next run for {}", routine.name),
            )
        })
}

pub async fn set_manual_checks_passed(pool: &PgPool, name: &str, passed: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO scheduled_routines (name, manual_checks_passed) VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE SET manual_checks_passed = EXCLUDED.manual_checks_passed",
    )
    .bind(name)
    .bind(passed)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

pub async fn is_enabled(pool: &PgPool, name: &str) -> Result<bool> {
    let row = sqlx::query("SELECT manual_checks_passed FROM scheduled_routines WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?;
    match row {
        Some(r) => r.try_get("manual_checks_passed").map_err(db_err),
        None => Ok(false),
    }
}

/// The next scheduled run, or `None` while the routine is disabled.
pub async fn scheduled_next_run(
    pool: &PgPool,
    routine: &Routine,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    if !is_enabled(pool, &routine.name).await? {
        return Ok(None);
    }
    next_run(routine, now).map(Some)
}

/// Execute one scheduled run, bounded by the routine's `max_runtime`.
/// Denied until `manual_checks_passed` is true for the routine.
pub async fn run_scheduled(
    pool: &PgPool,
    routine: &Routine,
    now: DateTime<Utc>,
) -> Result<ScheduledOutput> {
    if !is_enabled(pool, &routine.name).await? {
        tracing::warn!(routine = %routine.name, "scheduled run refused: manual checks not passed");
        return Err(PairError::new(
            ErrorCode::PolicyDenied,
            format!(
                "routine {} is disabled until its manual version has passed",
                routine.name
            ),
        ));
    }
    let work = async {
        match routine.kind {
            RoutineKind::Briefing => build_brief(pool, now).await.map(ScheduledOutput::Brief),
            RoutineKind::Review => build_review(pool, now).await.map(ScheduledOutput::Review),
        }
    };
    let output = tokio::time::timeout(routine.max_runtime, work)
        .await
        .map_err(|_| {
            PairError::new(
                ErrorCode::Internal,
                format!("routine {} exceeded its max runtime", routine.name),
            )
        })??;
    let next = next_run(routine, now)?;
    sqlx::query("UPDATE scheduled_routines SET last_run_at = $2, next_run_at = $3 WHERE name = $1")
        .bind(&routine.name)
        .bind(now)
        .bind(next)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(output)
}
