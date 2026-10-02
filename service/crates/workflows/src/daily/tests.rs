use super::model::{EventKind, LoopKind, LoopStatus, NewEvent, NewLoop};
use super::schedule::{self, parse_schedule, Routine, RoutineKind, ScheduledOutput};
use super::testdb::TestDb;
use super::{build_brief, build_review, next_run, store};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use pair_core::error::ErrorCode;
use serde::Deserialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Deserialize)]
struct FixtureLoop {
    key: String,
    title: String,
    owner: String,
    kind: LoopKind,
    status: LoopStatus,
    delegable: bool,
    due_at: Option<DateTime<Utc>>,
    source_ref: String,
    relationship: Option<String>,
}

#[derive(Deserialize)]
struct FixtureEvent {
    kind: EventKind,
    at: DateTime<Utc>,
    summary: String,
    #[serde(rename = "loop")]
    loop_key: Option<String>,
    source_ref: String,
}

#[derive(Deserialize)]
struct Week {
    loops: Vec<FixtureLoop>,
    events: Vec<FixtureEvent>,
}

struct Loaded {
    loops: HashMap<String, Uuid>,
    /// (kind, loop key) -> event id
    events: HashMap<(EventKind, String), Uuid>,
}

fn at(s: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

async fn load_week(pool: &PgPool) -> Result<Loaded, Box<dyn std::error::Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../evals/fixtures/week/week.json");
    let week: Week = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let mut loops = HashMap::new();
    for l in week.loops {
        let id = store::insert_loop(
            pool,
            &NewLoop {
                title: l.title,
                owner: l.owner,
                kind: l.kind,
                status: l.status,
                delegable: l.delegable,
                due_at: l.due_at,
                source_ref: l.source_ref,
                relationship: l.relationship,
                goal_id: None,
            },
        )
        .await?;
        loops.insert(l.key, id);
    }
    let mut events = HashMap::new();
    for e in week.events {
        let loop_id = e.loop_key.as_ref().and_then(|k| loops.get(k)).copied();
        let id = store::insert_event(
            pool,
            &NewEvent { kind: e.kind, occurred_at: e.at, summary: e.summary, loop_id, source_ref: e.source_ref },
        )
        .await?;
        if let Some(k) = e.loop_key {
            events.insert((e.kind, k), id);
        }
    }
    Ok(Loaded { loops, events })
}

fn loop_id(l: &Loaded, key: &str) -> Result<Uuid, String> {
    l.loops.get(key).copied().ok_or_else(|| format!("fixture loop {key} missing"))
}

fn event_id(l: &Loaded, kind: EventKind, key: &str) -> Result<Uuid, String> {
    l.events.get(&(kind, key.to_string())).copied().ok_or_else(|| format!("fixture event {key} missing"))
}

#[tokio::test]
async fn intent_is_not_completion() -> TestResult {
    let db = TestDb::new().await?;
    let w = load_week(&db.pool).await?;
    let acme = loop_id(&w, "acme")?;

    // Tuesday night, after the "I will send it tonight" intent.
    let review = build_review(&db.pool, at("2026-09-29T22:30:00+05:30")?).await?;
    assert!(review.completed.is_empty());
    assert!(review.worked_on.is_empty());
    assert!(review.unresolved.iter().any(|u| u.loop_id == acme));

    let intent = event_id(&w, EventKind::Intent, "acme")?;
    let err = store::complete_loop(&db.pool, acme, intent).await.err().ok_or("intent completed a loop")?;
    assert_eq!(err.code, ErrorCode::InvalidInput);
    assert_eq!(store::get_loop(&db.pool, acme).await?.status, LoopStatus::Open);
    Ok(())
}

#[tokio::test]
async fn overdue_commitment_remains_open() -> TestResult {
    let db = TestDb::new().await?;
    let w = load_week(&db.pool).await?;
    let acme = loop_id(&w, "acme")?;
    let friday = at("2026-10-02T07:30:00+05:30")?;

    let brief = build_brief(&db.pool, friday).await?;
    let p = brief.priorities.iter().find(|p| p.loop_id == acme).ok_or("overdue commitment dropped from brief")?;
    assert!(p.overdue);
    build_review(&db.pool, friday).await?;

    let l = store::get_loop(&db.pool, acme).await?;
    assert_eq!(l.status, LoopStatus::Open);
    assert!(l.completed_at.is_none());
    Ok(())
}

#[test]
fn schedule_uses_kolkata_timezone() -> TestResult {
    let cfg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/schedule.yaml");
    let routines = parse_schedule(&std::fs::read_to_string(cfg)?)?;
    let find = |n: &str| routines.iter().find(|r| r.name == n).cloned().ok_or(format!("routine {n} missing"));
    let (morning, evening) = (find("morning_brief")?, find("evening_review")?);
    assert_eq!(morning.timezone, chrono_tz::Asia::Kolkata);
    assert_eq!(evening.timezone, chrono_tz::Asia::Kolkata);

    // 07:30 IST = 02:00 UTC; 22:30 IST = 17:00 UTC.
    assert_eq!(next_run(&morning, at("2026-10-02T00:00:00Z")?)?, at("2026-10-02T02:00:00Z")?);
    assert_eq!(next_run(&morning, at("2026-10-02T02:00:00Z")?)?, at("2026-10-03T02:00:00Z")?);
    assert_eq!(next_run(&evening, at("2026-10-02T00:00:00Z")?)?, at("2026-10-02T17:00:00Z")?);
    // 23:00 IST on the 2nd is already the next local day's 07:30 window.
    assert_eq!(next_run(&morning, at("2026-10-02T17:30:00Z")?)?, at("2026-10-03T02:00:00Z")?);
    Ok(())
}

#[test]
fn schedule_next_run_is_dst_safe() -> TestResult {
    let ny = |time: &str| Routine {
        name: "ny".into(),
        kind: RoutineKind::Briefing,
        local_time: chrono::NaiveTime::parse_from_str(time, "%H:%M").unwrap_or_default(),
        timezone: Tz::America__New_York,
        max_runtime: std::time::Duration::from_secs(1),
    };
    // Spring forward 2026-03-08: 02:30 local does not exist; shifts to 03:00 EDT (07:00Z).
    assert_eq!(next_run(&ny("02:30"), at("2026-03-08T00:00:00Z")?)?, at("2026-03-08T07:00:00Z")?);
    // Fall back 2026-11-01: 01:30 occurs twice; earliest (EDT, 05:30Z) wins.
    assert_eq!(next_run(&ny("01:30"), at("2026-11-01T00:00:00Z")?)?, at("2026-11-01T05:30:00Z")?);
    Ok(())
}

#[tokio::test]
async fn scheduled_run_requires_manual_checks_passed() -> TestResult {
    let db = TestDb::new().await?;
    load_week(&db.pool).await?;
    let cfg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/schedule.yaml");
    let routines = parse_schedule(&std::fs::read_to_string(cfg)?)?;
    let morning = routines.first().ok_or("no routines")?;
    let now = at("2026-10-02T00:00:00Z")?;

    assert!(schedule::scheduled_next_run(&db.pool, morning, now).await?.is_none());
    let denied = schedule::run_scheduled(&db.pool, morning, now).await.err().ok_or("ran while disabled")?;
    assert_eq!(denied.code, ErrorCode::PolicyDenied);

    schedule::set_manual_checks_passed(&db.pool, &morning.name, true).await?;
    assert_eq!(schedule::scheduled_next_run(&db.pool, morning, now).await?, Some(at("2026-10-02T02:00:00Z")?));
    match schedule::run_scheduled(&db.pool, morning, now).await? {
        ScheduledOutput::Brief(b) => assert!(b.priorities.len() <= super::MAX_PRIORITIES),
        ScheduledOutput::Review(_) => return Err("expected a briefing".into()),
    }
    Ok(())
}

#[tokio::test]
async fn brief_prioritizes_commitments_over_inferences() -> TestResult {
    let db = TestDb::new().await?;
    let w = load_week(&db.pool).await?;
    let (pr42, recruiter) = (loop_id(&w, "pr42")?, loop_id(&w, "recruiter")?);
    store::complete_loop(&db.pool, pr42, event_id(&w, EventKind::ObservedCompletion, "pr42")?).await?;

    let brief = build_brief(&db.pool, at("2026-10-02T07:30:00+05:30")?).await?;
    let titles: Vec<&str> = brief.priorities.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, ["Send Acme proposal draft", "Prepare September invoice", "Book conference flights"]);
    // The inference is due earlier than every commitment yet must not displace one.
    assert!(brief.priorities.iter().all(|p| !p.inferred && p.loop_id != recruiter));
    assert!(brief.priorities.iter().all(|p| !p.source_ref.is_empty()));
    assert_eq!(brief.meetings.len(), 2);
    assert_eq!(brief.blockers.len(), 1);
    assert!(brief.delegations.len() <= super::MAX_DELEGATIONS);
    Ok(())
}

#[tokio::test]
async fn brief_fills_spare_slots_with_labelled_inferences() -> TestResult {
    let db = TestDb::new().await?;
    let w = load_week(&db.pool).await?;
    for key in ["acme", "pr42", "invoice", "flights"] {
        let id = loop_id(&w, key)?;
        sqlx::query("UPDATE open_loops SET status = 'dropped' WHERE id = $1").bind(id).execute(&db.pool).await?;
    }
    let brief = build_brief(&db.pool, at("2026-10-02T07:30:00+05:30")?).await?;
    assert_eq!(brief.priorities.len(), 1);
    assert!(brief.priorities.iter().all(|p| p.inferred));
    Ok(())
}

#[tokio::test]
async fn review_records_only_observed_completion() -> TestResult {
    let db = TestDb::new().await?;
    let w = load_week(&db.pool).await?;
    let (pr42, invoice) = (loop_id(&w, "pr42")?, loop_id(&w, "invoice")?);
    store::complete_loop(&db.pool, pr42, event_id(&w, EventKind::ObservedCompletion, "pr42")?).await?;

    // Thursday: invoice had stated intent only; PR 42 was observed complete.
    let thu = build_review(&db.pool, at("2026-10-01T22:30:00+05:30")?).await?;
    assert_eq!(thu.completed.len(), 1);
    assert_eq!(thu.completed[0].loop_id, Some(pr42));
    assert_eq!(thu.worked_on.len(), 1);
    assert!(thu.unresolved.iter().any(|u| u.loop_id == invoice));
    assert!(thu.unresolved.iter().all(|u| u.loop_id != pr42));
    assert!(thu.completed.iter().chain(&thu.worked_on).all(|i| !i.summary.contains("Plan to")));

    // Friday: nothing was completed; the day's decision is proposed, not accepted.
    let fri = build_review(&db.pool, at("2026-10-02T22:30:00+05:30")?).await?;
    assert!(fri.completed.is_empty());
    assert_eq!(fri.proposed_decisions.len(), 1);
    Ok(())
}
