use super::brief::{select_priorities, Priority};
use super::model::{EventKind, EventRecord, LoopStatus};
use super::{kolkata_day_bounds, store};
use chrono::{DateTime, Utc};
use pair_core::error::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedItem {
    pub loop_id: Option<Uuid>,
    pub summary: String,
    pub source_ref: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unresolved {
    pub loop_id: Uuid,
    pub title: String,
    pub source_ref: String,
    pub due_at: Option<DateTime<Utc>>,
    pub overdue: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub generated_at: DateTime<Utc>,
    /// Only items backed by an `observed_completion` event.
    pub completed: Vec<ObservedItem>,
    /// Only items backed by an `observed_work` event.
    pub worked_on: Vec<ObservedItem>,
    /// Decisions recorded today, proposed for the memory inbox (never auto-accepted).
    pub proposed_decisions: Vec<ObservedItem>,
    /// Every commitment still open or blocked, including those with stated intent.
    pub unresolved: Vec<Unresolved>,
    pub tomorrow: Vec<Priority>,
}

fn item(e: EventRecord) -> ObservedItem {
    ObservedItem {
        loop_id: e.loop_id,
        summary: e.summary,
        source_ref: e.source_ref,
        at: e.occurred_at,
    }
}

/// Summarise the Asia/Kolkata day containing `now`. Intent events are deliberately
/// ignored: stating that something will be done is not evidence that it was.
pub async fn build_review(pool: &PgPool, now: DateTime<Utc>) -> Result<Review> {
    let (start, end) = kolkata_day_bounds(now);
    let events = store::events_between(pool, start, end).await?;
    let mut completed = Vec::new();
    let mut worked_on = Vec::new();
    let mut proposed_decisions = Vec::new();
    for e in events {
        match e.kind {
            EventKind::ObservedCompletion => completed.push(item(e)),
            EventKind::ObservedWork => worked_on.push(item(e)),
            EventKind::Decision => proposed_decisions.push(item(e)),
            EventKind::Intent | EventKind::Meeting => {}
        }
    }
    let loops = store::unresolved_loops(pool).await?;
    let unresolved = loops
        .iter()
        .filter(|l| l.kind == super::model::LoopKind::Commitment)
        .map(|l| Unresolved {
            loop_id: l.id,
            title: l.title.clone(),
            source_ref: l.source_ref.clone(),
            due_at: l.due_at,
            overdue: l.due_at.is_some_and(|d| d < now) && l.status != LoopStatus::Done,
        })
        .collect();
    let tomorrow = select_priorities(&loops, end);
    tracing::info!(
        completed = completed.len(),
        worked = worked_on.len(),
        "review built"
    );
    Ok(Review {
        generated_at: now,
        completed,
        worked_on,
        proposed_decisions,
        unresolved,
        tomorrow,
    })
}
