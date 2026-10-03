//! Inbox review flows: reject, edit (replacement candidate) and correct (supersede accepted).
use crate::{
    accept::{accept_in_tx, record_conflict},
    audit,
    contradiction::find_links,
    error::{db_err, invalid, not_found},
    inbox::{insert_candidate, prepare, NewCandidateRow},
    model::{CandidateDraft, MemoryRecord, VerifiedSpans},
    policy,
    store::{AcceptMode, PgMemory},
};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{CandidateId, MemoryId, SourceId},
    types::{EvidenceRef, MemoryCandidate},
};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

const EDITED_REASON: &str = "edited";
const CORRECTION_REASON: &str = "correction";

/// Fields an editor may change. Original evidence is always carried over; `extra_evidence` adds.
#[derive(Debug, Clone, Default)]
pub struct EditPatch {
    pub content: Option<String>,
    pub kind: Option<String>,
    pub project: Option<String>,
    pub inferred: Option<bool>,
    pub reason: Option<String>,
    pub extra_evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Clone)]
pub struct CorrectionPatch {
    pub content: String,
    pub extra_evidence: Vec<EvidenceRef>,
    /// Keep the original accepted beside the correction (recorded as a conflict) instead of
    /// superseding it.
    pub keep_both: bool,
}

struct OriginalCandidate {
    kind: String,
    content: String,
    project: Option<String>,
    inferred: bool,
    reason: Option<String>,
    topic: Option<String>,
    extraction_version: String,
    evidence: Vec<EvidenceRef>,
    verified: VerifiedSpans,
}

async fn load_original(conn: &mut PgConnection, id: CandidateId) -> Result<OriginalCandidate> {
    let row = sqlx::query(
        "SELECT kind, content, project, inferred, reason, topic_key, state, extraction_version \
         FROM memory_candidates WHERE id = $1 FOR UPDATE",
    )
    .bind(id.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)?
    .ok_or_else(|| not_found("candidate", id))?;
    let state: String = row.try_get("state").map_err(db_err)?;
    if state != "pending" {
        return Err(PairError::new(
            ErrorCode::Conflict,
            format!("candidate {id} is {state}, not pending"),
        ));
    }
    let evidence = sqlx::query(
        "SELECT e.source_id, e.span, e.span_verified FROM memory_candidate_evidence e JOIN sources s ON s.id = e.source_id \
         WHERE e.candidate_id = $1 AND s.deletion_state = 'active' ORDER BY e.id",
    )
    .bind(id.0)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;
    ensure_evidence_alive(
        &mut *conn,
        "candidate_id",
        "memory_candidate_evidence",
        id.0,
    )
    .await?;
    let (evidence, verified) = split_evidence(&evidence)?;
    Ok(OriginalCandidate {
        kind: row.try_get("kind").map_err(db_err)?,
        content: row.try_get("content").map_err(db_err)?,
        project: row.try_get("project").map_err(db_err)?,
        inferred: row.try_get("inferred").map_err(db_err)?,
        reason: row.try_get("reason").map_err(db_err)?,
        topic: row.try_get("topic_key").map_err(db_err)?,
        extraction_version: row.try_get("extraction_version").map_err(db_err)?,
        evidence,
        verified,
    })
}

/// The record had evidence and none of it is on an active source: its text derives only from
/// deleted sources and must not be carried forward (an edit would resurrect it).
async fn ensure_evidence_alive(
    conn: &mut PgConnection,
    fk_column: &str,
    table: &str,
    id: Uuid,
) -> Result<()> {
    let (total, active): (i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*), count(*) FILTER (WHERE s.deletion_state = 'active') FROM {table} e \
         JOIN sources s ON s.id = e.source_id WHERE e.{fk_column} = $1"
    ))
    .bind(id)
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    if total > 0 && active == 0 {
        return Err(PairError::new(
            ErrorCode::SourceDeleted,
            format!("{id} was derived only from deleted sources"),
        ));
    }
    Ok(())
}

/// Evidence refs plus the set of (source, span) pairs flagged `span_verified`.
fn split_evidence(rows: &[sqlx::postgres::PgRow]) -> Result<(Vec<EvidenceRef>, VerifiedSpans)> {
    let mut evidence = Vec::with_capacity(rows.len());
    let mut verified = VerifiedSpans::new();
    for r in rows {
        let ev = EvidenceRef {
            source: SourceId(r.try_get("source_id").map_err(db_err)?),
            span: r.try_get("span").map_err(db_err)?,
        };
        if r.try_get::<bool, _>("span_verified").map_err(db_err)? {
            verified.insert((ev.source, ev.span.clone()));
        }
        evidence.push(ev);
    }
    Ok((evidence, verified))
}

impl PgMemory {
    pub async fn reject_candidate(&self, id: CandidateId, actor: &str, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let updated = sqlx::query(
            "UPDATE memory_candidates SET state = 'rejected', reviewed_by = $2, reviewed_at = $3, \
             review_reasons = array_append(review_reasons, $4) WHERE id = $1 AND state = 'pending'",
        )
        .bind(id.0)
        .bind(actor)
        .bind((self.clock)())
        .bind(format!("rejected: {reason}"))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if updated == 0 {
            let exists: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM memory_candidates WHERE id = $1")
                    .bind(id.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?;
            return Err(match exists {
                Some(_) => PairError::new(
                    ErrorCode::Conflict,
                    format!("candidate {id} is not pending"),
                ),
                None => not_found("candidate", id),
            });
        }
        tx.commit().await.map_err(db_err)?;
        tracing::info!(candidate = %id, actor, "candidate rejected");
        Ok(())
    }

    /// Replace a pending candidate with an edited one. The original keeps its text and evidence
    /// (state `edited`, `replaced_by` set); the replacement carries all evidence plus any new.
    pub async fn edit_candidate(
        &self,
        id: CandidateId,
        actor: &str,
        patch: EditPatch,
    ) -> Result<CandidateId> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let original = load_original(&mut tx, id).await?;
        let content_changed = patch
            .content
            .as_deref()
            .is_some_and(|c| c.trim() != original.content);
        let mut evidence = original.evidence;
        evidence.extend(patch.extra_evidence);

        let mut draft = CandidateDraft::new(MemoryCandidate {
            kind: patch.kind.unwrap_or(original.kind),
            content: patch.content.unwrap_or(original.content),
            project: patch.project.or(original.project),
            inferred: patch.inferred.unwrap_or(original.inferred),
            evidence,
        });
        draft.reason = patch.reason.or(original.reason);
        draft.topic = if content_changed {
            None
        } else {
            original.topic
        };
        draft.extraction_version = original.extraction_version;
        draft.carried_verified = original.verified;

        let prepared = prepare(&mut tx, draft, Some(&format!("edit:{id}"))).await?;
        let c = &prepared.draft.candidate;
        let links = find_links(
            &mut tx,
            &c.kind,
            c.project.as_deref(),
            prepared.topic.as_deref(),
            &prepared.normalized,
            (self.clock)(),
            Some(id.0),
        )
        .await?;
        let (mem_links, cand_links) = (links.memories, links.candidates);
        let mut assessment = policy::assess(
            &c.kind,
            c.inferred,
            &c.content,
            &prepared.facts,
            !mem_links.is_empty() || !cand_links.is_empty(),
            prepared.topic.is_some(),
        );
        // A human edit is never auto-accepted, even if the edited text would otherwise qualify.
        assessment
            .review_reasons
            .retain(|r| r != "not_a_preference");
        assessment.review_reasons.push(EDITED_REASON.to_string());
        let replacement = insert_candidate(
            &mut tx,
            NewCandidateRow {
                prepared: &prepared,
                review_reasons: &assessment.review_reasons,
                contradicts_memories: &mem_links,
                contradicts_candidates: &cand_links,
                edited_from: Some(id),
            },
        )
        .await?;
        sqlx::query(
            "UPDATE memory_candidates SET state = 'edited', replaced_by = $2, reviewed_by = $3, reviewed_at = $4 WHERE id = $1",
        )
        .bind(id.0)
        .bind(replacement.0)
        .bind(actor)
        .bind((self.clock)())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        tracing::info!(original = %id, replacement = %replacement, actor, "candidate edited");
        Ok(replacement)
    }

    /// Correct an accepted memory: a replacement is accepted and supersedes the original, which
    /// stays in history with its evidence. The replacement carries the original evidence.
    pub async fn correct_memory(
        &self,
        id: MemoryId,
        actor: &str,
        patch: CorrectionPatch,
    ) -> Result<MemoryId> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let original = sqlx::query(
            "SELECT kind, project, confidence, topic_key FROM memories WHERE id = $1 AND status = 'accepted' \
             AND invalidated_reason IS NULL FOR UPDATE",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| PairError::new(ErrorCode::Conflict, format!("memory {id} is not an active accepted memory")))?;
        let evidence_rows = sqlx::query(
            "SELECT e.source_id, e.span, e.span_verified FROM memory_evidence e JOIN sources s ON s.id = e.source_id \
             WHERE e.memory_id = $1 AND s.deletion_state = 'active' ORDER BY e.id",
        )
        .bind(id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        ensure_evidence_alive(&mut tx, "memory_id", "memory_evidence", id.0).await?;
        let (mut evidence, verified) = split_evidence(&evidence_rows)?;
        evidence.extend(patch.extra_evidence);
        if patch.content.trim().is_empty() {
            return Err(invalid("correction content must not be empty"));
        }

        let mut draft = CandidateDraft::new(MemoryCandidate {
            kind: original.try_get("kind").map_err(db_err)?,
            content: patch.content,
            project: original.try_get("project").map_err(db_err)?,
            inferred: original
                .try_get::<String, _>("confidence")
                .map_err(db_err)?
                == "inferred",
            evidence,
        });
        draft.topic = original.try_get("topic_key").map_err(db_err)?;
        draft.reason = Some(CORRECTION_REASON.to_string());
        draft.carried_verified = verified;
        let prepared = prepare(&mut tx, draft, Some(&id.to_string())).await?;
        let candidate = insert_candidate(
            &mut tx,
            NewCandidateRow {
                prepared: &prepared,
                review_reasons: &[CORRECTION_REASON.to_string()],
                contradicts_memories: &[],
                contradicts_candidates: &[],
                edited_from: None,
            },
        )
        .await?;
        let new_id = accept_in_tx(
            &mut tx,
            self.clock,
            candidate,
            actor,
            AcceptMode {
                supersedes: (!patch.keep_both).then_some(id),
                keep_both: patch.keep_both,
            },
        )
        .await?;
        if patch.keep_both {
            record_conflict(&mut tx, id.0, new_id.0, actor).await?;
        }
        audit::record(
            &mut tx,
            actor,
            "memory.corrected",
            "memory",
            &id.to_string(),
            serde_json::json!({ "replacement": new_id.to_string() }),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        tracing::info!(memory = %id, replacement = %new_id, actor, "memory corrected");
        Ok(new_id)
    }

    /// The memory currently active at the end of `id`'s supersession chain.
    pub async fn current_version(&self, id: MemoryId) -> Result<MemoryRecord> {
        let chain = self.memory_chain(id).await?;
        chain
            .into_iter()
            .last()
            .ok_or_else(|| not_found("memory", id))
    }
}
