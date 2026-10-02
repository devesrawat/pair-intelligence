//! Acceptance of a candidate into a memory: evidence check, policy gate, contradiction
//! resolution, provenance rows, chunks and audit, all inside the caller's transaction.
use crate::{
    audit,
    error::{db_err, not_found},
    inbox::find_memory_contradictions,
    model::parse_trust,
    policy,
    store::{insert_evidence_and_chunks, AcceptMode},
};
use chrono::{DateTime, Utc};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{CandidateId, MemoryId, SourceId},
    types::{EvidenceRef, TrustClass},
};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// Importance grows with repetition but is capped: repetition is not proof of truth.
const MAX_REPETITION_IMPORTANCE: i32 = 5;

struct CandidateRow {
    kind: String,
    content: String,
    normalized: String,
    topic: Option<String>,
    project: Option<String>,
    inferred: bool,
    seen_count: i32,
    observed_at: Option<DateTime<Utc>>,
    valid_from: Option<DateTime<Utc>>,
    valid_to: Option<DateTime<Utc>>,
    extraction_version: String,
}

pub(crate) async fn accept_in_tx(
    conn: &mut PgConnection,
    clock: fn() -> DateTime<Utc>,
    id: CandidateId,
    actor: &str,
    mode: AcceptMode,
) -> Result<MemoryId> {
    let row = sqlx::query(
        "SELECT kind, content, normalized_content, topic_key, project, inferred, state, seen_count, accepted_memory_id, \
         observed_at, valid_from, valid_to, extraction_version FROM memory_candidates WHERE id = $1 FOR UPDATE",
    )
    .bind(id.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)?
    .ok_or_else(|| not_found("candidate", id))?;
    let state: String = row.try_get("state").map_err(db_err)?;
    if state == "accepted" {
        // Idempotent: an auto-accepted candidate may be accepted again by a reviewer.
        let existing: Option<Uuid> = row.try_get("accepted_memory_id").map_err(db_err)?;
        return existing.map(MemoryId).ok_or_else(|| {
            PairError::new(ErrorCode::Internal, "accepted candidate has no memory")
        });
    }
    if state != "pending" {
        return Err(PairError::new(
            ErrorCode::Conflict,
            format!("candidate {id} is {state}, not pending"),
        ));
    }
    let cand = CandidateRow {
        kind: row.try_get("kind").map_err(db_err)?,
        content: row.try_get("content").map_err(db_err)?,
        normalized: row.try_get("normalized_content").map_err(db_err)?,
        topic: row.try_get("topic_key").map_err(db_err)?,
        project: row.try_get("project").map_err(db_err)?,
        inferred: row.try_get("inferred").map_err(db_err)?,
        seen_count: row.try_get("seen_count").map_err(db_err)?,
        observed_at: row.try_get("observed_at").map_err(db_err)?,
        valid_from: row.try_get("valid_from").map_err(db_err)?,
        valid_to: row.try_get("valid_to").map_err(db_err)?,
        extraction_version: row.try_get("extraction_version").map_err(db_err)?,
    };

    let (live, trusts) = live_evidence(conn, id).await?;
    policy::check_accept(&cand.kind, &cand.content, &trusts)?;

    let now = clock();
    let contradicted = match &cand.topic {
        Some(topic) => {
            find_memory_contradictions(
                conn,
                &cand.kind,
                cand.project.as_deref(),
                topic,
                &cand.normalized,
                now,
            )
            .await?
        }
        None => Vec::new(),
    };
    let unresolved: Vec<Uuid> = contradicted
        .iter()
        .map(|m| m.0)
        .filter(|m| Some(*m) != mode.supersedes.map(|s| s.0))
        .collect();
    if !unresolved.is_empty() && !mode.keep_both {
        return Err(PairError::new(
            ErrorCode::Conflict,
            format!(
                "candidate {id} contradicts {} accepted memories; supersede one or keep both",
                unresolved.len()
            ),
        ));
    }

    let observed_at = cand.observed_at.unwrap_or(now);
    let valid_from = cand.valid_from.unwrap_or(observed_at);
    let memory_id = MemoryId::new();
    if let Some(old) = mode.supersedes {
        close_superseded(conn, old, valid_from, actor).await?;
    }
    sqlx::query(
        "INSERT INTO memories (id, kind, status, content, normalized_content, topic_key, project, valid_from, valid_to, \
         observed_at, confidence, importance, supersedes_id, accepted_by) \
         VALUES ($1, $2, 'accepted', $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(memory_id.0)
    .bind(&cand.kind)
    .bind(&cand.content)
    .bind(&cand.normalized)
    .bind(&cand.topic)
    .bind(&cand.project)
    .bind(valid_from)
    .bind(cand.valid_to)
    .bind(observed_at)
    .bind(if cand.inferred { "inferred" } else { "observed" })
    .bind(i16::try_from(cand.seen_count.clamp(1, MAX_REPETITION_IMPORTANCE)).unwrap_or(1))
    .bind(mode.supersedes.map(|m| m.0))
    .bind(actor)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    insert_evidence_and_chunks(
        conn,
        memory_id,
        &cand.content,
        &live,
        &cand.extraction_version,
    )
    .await?;

    if mode.keep_both {
        for other in unresolved {
            record_conflict(conn, memory_id.0, other, actor).await?;
        }
    }
    sqlx::query(
        "UPDATE memory_candidates SET state = 'accepted', accepted_memory_id = $2, reviewed_by = $3, reviewed_at = $4, auto_accepted = ($3 = $5) WHERE id = $1",
    )
    .bind(id.0)
    .bind(memory_id.0)
    .bind(actor)
    .bind(now)
    .bind(policy::AUTO_ACCEPT_ACTOR)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    audit::record(
        conn,
        actor,
        "memory.accepted",
        "memory",
        &memory_id.to_string(),
        serde_json::json!({
            "candidate": id.to_string(), "kind": cand.kind, "inferred": cand.inferred,
            "supersedes": mode.supersedes.map(|m| m.to_string()), "keep_both": mode.keep_both,
        }),
    )
    .await?;
    Ok(memory_id)
}

/// Evidence on active sources plus the trust classes of those sources. Errors when the
/// candidate has no evidence at all, or only evidence from deleted sources.
async fn live_evidence(
    conn: &mut PgConnection,
    id: CandidateId,
) -> Result<(Vec<EvidenceRef>, Vec<TrustClass>)> {
    let rows = sqlx::query(
        "SELECT e.source_id, e.span, s.deletion_state, s.trust FROM memory_candidate_evidence e \
         JOIN sources s ON s.id = e.source_id WHERE e.candidate_id = $1 ORDER BY e.id",
    )
    .bind(id.0)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;
    if rows.is_empty() {
        return Err(PairError::new(
            ErrorCode::MemoryNoEvidence,
            "an accepted memory requires at least one evidence reference",
        ));
    }
    let mut live = Vec::with_capacity(rows.len());
    let mut trusts = Vec::with_capacity(rows.len());
    for row in &rows {
        if row.try_get::<String, _>("deletion_state").map_err(db_err)? == "active" {
            live.push(EvidenceRef {
                source: SourceId(row.try_get("source_id").map_err(db_err)?),
                span: row.try_get("span").map_err(db_err)?,
            });
            trusts.push(parse_trust(
                &row.try_get::<String, _>("trust").map_err(db_err)?,
            )?);
        }
    }
    if live.is_empty() {
        return Err(PairError::new(
            ErrorCode::SourceDeleted,
            "all evidence sources of this candidate are deleted",
        ));
    }
    Ok((live, trusts))
}

async fn record_conflict(conn: &mut PgConnection, a: Uuid, b: Uuid, actor: &str) -> Result<()> {
    let (low, high) = if a < b { (a, b) } else { (b, a) };
    sqlx::query("INSERT INTO memory_conflicts (memory_a, memory_b, resolved_by) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(low)
        .bind(high)
        .bind(actor)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    audit::record(
        conn,
        actor,
        "memory.conflict_kept",
        "memory",
        &a.to_string(),
        serde_json::json!({ "other": b.to_string() }),
    )
    .await
}

async fn close_superseded(
    conn: &mut PgConnection,
    old: MemoryId,
    new_valid_from: DateTime<Utc>,
    actor: &str,
) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE memories SET status = 'superseded', \
                valid_to = GREATEST(valid_from, LEAST(COALESCE(valid_to, $2), $2)) \
         WHERE id = $1 AND status = 'accepted' AND invalidated_reason IS NULL \
           AND NOT EXISTS (SELECT 1 FROM memories n WHERE n.supersedes_id = $1)",
    )
    .bind(old.0)
    .bind(new_valid_from)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?
    .rows_affected();
    if updated == 0 {
        return Err(PairError::new(
            ErrorCode::Conflict,
            format!("memory {old} cannot be superseded (not an active accepted memory)"),
        ));
    }
    audit::record(
        conn,
        actor,
        "memory.superseded",
        "memory",
        &old.to_string(),
        serde_json::json!({}),
    )
    .await
}
