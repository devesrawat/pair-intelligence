use super::model::{EventKind, EventRecord, LoopKind, LoopStatus, NewEvent, NewLoop, OpenLoop};
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use sqlx::{postgres::PgRow, PgPool, Row};
use uuid::Uuid;

/// Map a database error to a structured internal error, logging the detail.
pub(crate) fn db_err(e: sqlx::Error) -> PairError {
    tracing::error!(error = %e, "database error");
    PairError::new(ErrorCode::Internal, format!("database error: {e}"))
}

pub async fn insert_goal(
    pool: &PgPool,
    title: &str,
    owner: &str,
    due_at: Option<DateTime<Utc>>,
    evidence: &[String],
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let evidence = serde_json::json!(evidence);
    sqlx::query("INSERT INTO goals (id, title, owner, due_at, evidence) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(title)
        .bind(owner)
        .bind(due_at)
        .bind(evidence)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(id)
}

pub async fn insert_loop(pool: &PgPool, l: &NewLoop) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let evidence = serde_json::json!([l.source_ref]);
    sqlx::query(
        "INSERT INTO open_loops (id, goal_id, relationship, title, owner, kind, status, delegable, due_at, source_ref, evidence)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(l.goal_id)
    .bind(&l.relationship)
    .bind(&l.title)
    .bind(&l.owner)
    .bind(l.kind.as_str())
    .bind(l.status.as_str())
    .bind(l.delegable)
    .bind(l.due_at)
    .bind(&l.source_ref)
    .bind(evidence)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(id)
}

pub async fn insert_event(pool: &PgPool, e: &NewEvent) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO daily_events (id, occurred_at, kind, summary, loop_id, source_ref) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(e.occurred_at)
    .bind(e.kind.as_str())
    .bind(&e.summary)
    .bind(e.loop_id)
    .bind(&e.source_ref)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(id)
}

/// Close a loop. Only an `observed_completion` event for that same loop is accepted;
/// intent and plain work events are rejected.
pub async fn complete_loop(pool: &PgPool, loop_id: Uuid, event_id: Uuid) -> Result<()> {
    let row = sqlx::query("SELECT kind, loop_id, occurred_at FROM daily_events WHERE id = $1")
        .bind(event_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| PairError::new(ErrorCode::NotFound, "completion event not found"))?;
    let kind = EventKind::parse(&row.try_get::<String, _>("kind").map_err(db_err)?)?;
    let event_loop: Option<Uuid> = row.try_get("loop_id").map_err(db_err)?;
    let at: DateTime<Utc> = row.try_get("occurred_at").map_err(db_err)?;
    if kind != EventKind::ObservedCompletion || event_loop != Some(loop_id) {
        return Err(PairError::new(
            ErrorCode::InvalidInput,
            "a loop can only be completed by an observed_completion event recorded for it",
        ));
    }
    let done = sqlx::query(
        "UPDATE open_loops SET status = 'done', completed_at = $2 WHERE id = $1 AND status IN ('open', 'blocked')",
    )
    .bind(loop_id)
    .bind(at)
    .execute(pool)
    .await
    .map_err(db_err)?;
    if done.rows_affected() == 0 {
        return Err(PairError::new(ErrorCode::Conflict, "loop is not open"));
    }
    Ok(())
}

fn loop_from_row(r: &PgRow) -> Result<OpenLoop> {
    Ok(OpenLoop {
        id: r.try_get("id").map_err(db_err)?,
        title: r.try_get("title").map_err(db_err)?,
        owner: r.try_get("owner").map_err(db_err)?,
        kind: LoopKind::parse(&r.try_get::<String, _>("kind").map_err(db_err)?)?,
        status: LoopStatus::parse(&r.try_get::<String, _>("status").map_err(db_err)?)?,
        delegable: r.try_get("delegable").map_err(db_err)?,
        due_at: r.try_get("due_at").map_err(db_err)?,
        source_ref: r.try_get("source_ref").map_err(db_err)?,
        relationship: r.try_get("relationship").map_err(db_err)?,
        completed_at: r.try_get("completed_at").map_err(db_err)?,
    })
}

pub async fn get_loop(pool: &PgPool, id: Uuid) -> Result<OpenLoop> {
    let row = sqlx::query("SELECT * FROM open_loops WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| PairError::new(ErrorCode::NotFound, "loop not found"))?;
    loop_from_row(&row)
}

/// Loops that are still unresolved (open or blocked), oldest due date first.
pub async fn unresolved_loops(pool: &PgPool) -> Result<Vec<OpenLoop>> {
    let rows = sqlx::query(
        "SELECT * FROM open_loops WHERE status IN ('open', 'blocked') ORDER BY due_at NULLS LAST, created_at, id",
    )
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    rows.iter().map(loop_from_row).collect()
}

/// Events with `start <= occurred_at < end`, chronological.
pub async fn events_between(pool: &PgPool, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<EventRecord>> {
    let rows = sqlx::query(
        "SELECT id, kind, occurred_at, summary, loop_id, source_ref FROM daily_events
         WHERE occurred_at >= $1 AND occurred_at < $2 ORDER BY occurred_at, id",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    rows.iter()
        .map(|r| {
            Ok(EventRecord {
                id: r.try_get("id").map_err(db_err)?,
                kind: EventKind::parse(&r.try_get::<String, _>("kind").map_err(db_err)?)?,
                occurred_at: r.try_get("occurred_at").map_err(db_err)?,
                summary: r.try_get("summary").map_err(db_err)?,
                loop_id: r.try_get("loop_id").map_err(db_err)?,
                source_ref: r.try_get("source_ref").map_err(db_err)?,
            })
        })
        .collect()
}
