use super::model::{EventKind, LoopKind, LoopStatus, OpenLoop};
use super::{kolkata_day_bounds, store, MAX_DELEGATIONS, MAX_PRIORITIES};
use chrono::{DateTime, Utc};
use pair_core::error::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Priority {
    pub loop_id: Uuid,
    pub title: String,
    pub source_ref: String,
    pub due_at: Option<DateTime<Utc>>,
    pub overdue: bool,
    /// True when the item comes from an inference rather than a recorded commitment.
    pub inferred: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    pub at: DateTime<Utc>,
    pub title: String,
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blocker {
    pub loop_id: Uuid,
    pub title: String,
    pub source_ref: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delegation {
    pub loop_id: Uuid,
    pub title: String,
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub generated_at: DateTime<Utc>,
    pub priorities: Vec<Priority>,
    pub meetings: Vec<Meeting>,
    pub blockers: Vec<Blocker>,
    pub delegations: Vec<Delegation>,
}

fn to_priority(l: &OpenLoop, now: DateTime<Utc>) -> Priority {
    Priority {
        loop_id: l.id,
        title: l.title.clone(),
        source_ref: l.source_ref.clone(),
        due_at: l.due_at,
        overdue: l.due_at.is_some_and(|d| d < now),
        inferred: l.kind == LoopKind::Inference,
    }
}

/// Pick up to `MAX_PRIORITIES` open items. Commitments always outrank inferences,
/// regardless of due date; within a class, earliest due first (overdue sorts first,
/// undated last). Blocked items are reported as blockers, not priorities.
pub fn select_priorities(loops: &[OpenLoop], now: DateTime<Utc>) -> Vec<Priority> {
    let mut open: Vec<&OpenLoop> = loops
        .iter()
        .filter(|l| l.status == LoopStatus::Open)
        .collect();
    open.sort_by_key(|l| {
        (
            l.kind == LoopKind::Inference,
            l.due_at.is_none(),
            l.due_at,
            l.id,
        )
    });
    open.into_iter()
        .take(MAX_PRIORITIES)
        .map(|l| to_priority(l, now))
        .collect()
}

pub async fn build_brief(pool: &PgPool, now: DateTime<Utc>) -> Result<Brief> {
    let loops = store::unresolved_loops(pool).await?;
    let priorities = select_priorities(&loops, now);
    let (start, end) = kolkata_day_bounds(now);
    let meetings = store::events_between(pool, start, end)
        .await?
        .into_iter()
        .filter(|e| e.kind == EventKind::Meeting)
        .map(|e| Meeting {
            at: e.occurred_at,
            title: e.summary,
            source_ref: e.source_ref,
        })
        .collect();
    let blockers = loops
        .iter()
        .filter(|l| l.status == LoopStatus::Blocked)
        .map(|l| Blocker {
            loop_id: l.id,
            title: l.title.clone(),
            source_ref: l.source_ref.clone(),
            reason: l.relationship.clone(),
        })
        .collect();
    let delegations = loops
        .iter()
        .filter(|l| {
            l.delegable
                && l.status == LoopStatus::Open
                && l.kind == LoopKind::Commitment
                && !priorities.iter().any(|p| p.loop_id == l.id)
        })
        .take(MAX_DELEGATIONS)
        .map(|l| Delegation {
            loop_id: l.id,
            title: l.title.clone(),
            source_ref: l.source_ref.clone(),
        })
        .collect();
    tracing::info!(priorities = ?priorities.len(), "briefing built");
    Ok(Brief {
        generated_at: now,
        priorities,
        meetings,
        blockers,
        delegations,
    })
}
