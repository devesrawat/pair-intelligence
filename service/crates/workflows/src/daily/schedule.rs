//! Scheduled routines: parsed from `config/schedule.yaml`, run as bounded jobs, and
//! enabled only after the manual versions have passed their checks.
use super::brief::{build_brief, Brief};
use super::review::{build_review, Review};
use super::store::db_err;
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use pair_core::error::{ErrorCode, PairError, Result};
use sqlx::{PgPool, Row};
use std::time::Duration as StdDuration;

/// Days scanned forward when computing the next run (covers any DST gap/overlap).
const NEXT_RUN_SEARCH_DAYS: i64 = 3;
/// Step used to move past a nonexistent local time (spring-forward gap).
const GAP_STEP_MINUTES: i64 = 30;
/// Maximum total shift past a nonexistent local time.
const GAP_MAX_SHIFT_MINUTES: i64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Default)]
struct Draft {
    name: Option<String>,
    kind: Option<RoutineKind>,
    local_time: Option<NaiveTime>,
    timezone: Option<Tz>,
    max_secs: Option<u64>,
}

impl Draft {
    fn finish(self) -> Result<Routine> {
        let name = self.name.ok_or_else(|| invalid("routine missing name"))?;
        let missing = |f: &str| invalid(format!("routine {name} missing {f}"));
        Ok(Routine {
            kind: self.kind.ok_or_else(|| missing("kind"))?,
            local_time: self.local_time.ok_or_else(|| missing("local_time"))?,
            timezone: self.timezone.ok_or_else(|| missing("timezone"))?,
            max_runtime: StdDuration::from_secs(
                self.max_secs.ok_or_else(|| missing("max_runtime_secs"))?,
            ),
            name,
        })
    }

    fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "name" => self.name = Some(value.to_string()),
            "kind" => {
                self.kind = Some(match value {
                    "briefing" => RoutineKind::Briefing,
                    "review" => RoutineKind::Review,
                    other => return Err(invalid(format!("unknown routine kind {other}"))),
                })
            }
            "local_time" => {
                self.local_time = Some(
                    NaiveTime::parse_from_str(value, "%H:%M")
                        .map_err(|e| invalid(format!("bad local_time {value}: {e}")))?,
                )
            }
            "timezone" => {
                self.timezone = Some(
                    value
                        .parse::<Tz>()
                        .map_err(|e| invalid(format!("bad timezone {value}: {e}")))?,
                )
            }
            "max_runtime_secs" => {
                self.max_secs = Some(
                    value
                        .parse()
                        .map_err(|e| invalid(format!("bad max_runtime_secs {value}: {e}")))?,
                )
            }
            other => return Err(invalid(format!("unknown schedule key {other}"))),
        }
        Ok(())
    }
}

/// Parse the restricted `routines:` list format used by `config/schedule.yaml`.
pub fn parse_schedule(text: &str) -> Result<Vec<Routine>> {
    let mut routines = Vec::new();
    let mut current: Option<Draft> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line == "routines:" {
            continue;
        }
        let (item_start, body) = match line.strip_prefix("- ") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        if item_start {
            if let Some(done) = current.take() {
                routines.push(done.finish()?);
            }
            current = Some(Draft::default());
        }
        let (key, value) = body
            .split_once(':')
            .ok_or_else(|| invalid(format!("bad schedule line: {line}")))?;
        let value = value.trim().trim_matches('"');
        current
            .as_mut()
            .ok_or_else(|| invalid("schedule key outside a routine"))?
            .set(key.trim(), value)?;
    }
    if let Some(done) = current {
        routines.push(done.finish()?);
    }
    if routines.is_empty() {
        return Err(invalid("schedule defines no routines"));
    }
    Ok(routines)
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
