//! Memory inbox: proposal pipeline (normalise, dedupe, contradiction links, policy) and the
//! reviewer-facing views. Review actions (reject/edit/correct) live in `review`.
use crate::{
    error::{db_err, invalid, not_found},
    model::{parse_data_class, parse_trust, validate_kind, CandidateDraft},
    normalize::{normalize_content, topic_of},
    policy::{self, SourceFacts, AUTO_ACCEPT_ACTOR},
    store::{AcceptMode, PgMemory, MAX_CONTENT_CHARS},
};
use chrono::{DateTime, Utc};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{CandidateId, MemoryId, SourceId},
    types::TrustClass,
};
use serde::Serialize;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub use crate::review::{CorrectionPatch, EditPatch};

/// Result of proposing a candidate.
#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub id: CandidateId,
    /// True when an equivalent candidate (same normalised content and source identity) existed.
    pub duplicate: bool,
    pub needs_review: bool,
    pub review_reasons: Vec<String>,
    pub auto_accepted: Option<MemoryId>,
    pub contradicts_memories: Vec<MemoryId>,
    pub contradicts_candidates: Vec<CandidateId>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InboxEvidence {
    pub source: SourceId,
    pub source_kind: String,
    pub external_id: String,
    pub trust: TrustClass,
    pub span: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AffectedMemory {
    pub memory: MemoryId,
    pub kind: String,
    pub content: String,
    pub status: String,
}

/// What a reviewer sees: the proposed fact, evidence, reason and affected records.
#[derive(Debug, Clone, Serialize)]
pub struct InboxEntry {
    pub id: CandidateId,
    pub state: String,
    pub kind: String,
    pub content: String,
    pub project: Option<String>,
    /// False = stated/observed, true = concluded by the extractor.
    pub inferred: bool,
    pub reason: Option<String>,
    pub review_reasons: Vec<String>,
    pub needs_review: bool,
    pub seen_count: i32,
    pub evidence: Vec<InboxEvidence>,
    pub contradicts_memories: Vec<AffectedMemory>,
    pub contradicts_candidates: Vec<CandidateId>,
    pub edited_from: Option<CandidateId>,
    pub replaced_by: Option<CandidateId>,
    /// End of the `replaced_by` chain: the version that is currently active.
    pub active_replacement: Option<CandidateId>,
    pub accepted_memory: Option<MemoryId>,
    pub created_at: DateTime<Utc>,
}

/// A draft after validation, with derived fields.
pub(crate) struct Prepared {
    pub draft: CandidateDraft,
    pub normalized: String,
    pub topic: Option<String>,
    pub dedupe_key: String,
    pub facts: Vec<SourceFacts>,
}

pub(crate) async fn prepare(
    conn: &mut PgConnection,
    mut draft: CandidateDraft,
    key_salt: Option<&str>,
) -> Result<Prepared> {
    validate_kind(&draft.candidate.kind)?;
    draft.candidate.content = draft.candidate.content.trim().to_string();
    let content = &draft.candidate.content;
    if content.is_empty() || content.chars().count() > MAX_CONTENT_CHARS {
        return Err(invalid(format!(
            "candidate content must be 1..={MAX_CONTENT_CHARS} characters"
        )));
    }
    let normalized = normalize_content(content);
    let topic = draft
        .topic
        .as_deref()
        .map(normalize_content)
        .filter(|t| !t.is_empty())
        .or_else(|| topic_of(content));

    let ids: Vec<Uuid> = draft
        .candidate
        .evidence
        .iter()
        .map(|e| e.source.0)
        .collect();
    let rows = sqlx::query("SELECT DISTINCT id, kind, external_id, trust, data_class, deletion_state FROM sources WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
    let distinct: std::collections::BTreeSet<&Uuid> = ids.iter().collect();
    if rows.len() != distinct.len() {
        return Err(invalid("candidate references an unknown source"));
    }
    let mut identities = std::collections::BTreeSet::new();
    let mut facts = Vec::with_capacity(rows.len());
    for row in &rows {
        if row.try_get::<String, _>("deletion_state").map_err(db_err)? != "active" {
            return Err(PairError::new(
                ErrorCode::SourceDeleted,
                "candidate references a deleted source",
            ));
        }
        identities.insert(format!(
            "{}:{}",
            row.try_get::<String, _>("kind").map_err(db_err)?,
            row.try_get::<String, _>("external_id").map_err(db_err)?
        ));
        facts.push(SourceFacts {
            trust: parse_trust(&row.try_get::<String, _>("trust").map_err(db_err)?)?,
            data_class: parse_data_class(&row.try_get::<String, _>("data_class").map_err(db_err)?)?,
        });
    }
    let material = format!(
        "{}\n{}\n{}",
        key_salt.unwrap_or(""),
        normalized,
        identities.into_iter().collect::<Vec<_>>().join(",")
    );
    let dedupe_key: String =
        sqlx::query_scalar("SELECT encode(sha256(convert_to($1, 'UTF8')), 'hex')")
            .bind(material)
            .fetch_one(&mut *conn)
            .await
            .map_err(db_err)?;
    Ok(Prepared {
        draft,
        normalized,
        topic,
        dedupe_key,
        facts,
    })
}

/// Accepted, currently valid memories on the same topic with different content.
pub(crate) async fn find_memory_contradictions(
    conn: &mut PgConnection,
    kind: &str,
    project: Option<&str>,
    topic: &str,
    normalized: &str,
    as_of: DateTime<Utc>,
) -> Result<Vec<MemoryId>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM memories WHERE status = 'accepted' AND invalidated_reason IS NULL AND kind = $1 \
         AND project IS NOT DISTINCT FROM $2 AND topic_key = $3 AND normalized_content <> $4 \
         AND (valid_to IS NULL OR valid_to > $5) ORDER BY id",
    )
    .bind(kind)
    .bind(project)
    .bind(topic)
    .bind(normalized)
    .bind(as_of)
    .fetch_all(conn)
    .await
    .map_err(db_err)?;
    Ok(ids.into_iter().map(MemoryId).collect())
}

pub(crate) async fn find_candidate_contradictions(
    conn: &mut PgConnection,
    kind: &str,
    project: Option<&str>,
    topic: &str,
    normalized: &str,
    exclude: Option<Uuid>,
) -> Result<Vec<CandidateId>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM memory_candidates WHERE state = 'pending' AND kind = $1 AND project IS NOT DISTINCT FROM $2 \
         AND topic_key = $3 AND normalized_content <> $4 AND id IS DISTINCT FROM $5 ORDER BY id",
    )
    .bind(kind)
    .bind(project)
    .bind(topic)
    .bind(normalized)
    .bind(exclude)
    .fetch_all(conn)
    .await
    .map_err(db_err)?;
    Ok(ids.into_iter().map(CandidateId).collect())
}

pub(crate) struct NewCandidateRow<'a> {
    pub prepared: &'a Prepared,
    pub review_reasons: &'a [String],
    pub contradicts_memories: &'a [MemoryId],
    pub contradicts_candidates: &'a [CandidateId],
    pub edited_from: Option<CandidateId>,
}

pub(crate) async fn insert_candidate(
    conn: &mut PgConnection,
    row: NewCandidateRow<'_>,
) -> Result<CandidateId> {
    let p = row.prepared;
    let c = &p.draft.candidate;
    let id = CandidateId::new();
    let memories: Vec<Uuid> = row.contradicts_memories.iter().map(|m| m.0).collect();
    let candidates: Vec<Uuid> = row.contradicts_candidates.iter().map(|m| m.0).collect();
    sqlx::query(
        "INSERT INTO memory_candidates (id, kind, content, normalized_content, topic_key, reason, project, inferred, \
         observed_at, valid_from, valid_to, extraction_version, dedupe_key, needs_review, review_reasons, \
         contradicts_memories, contradicts_candidates, edited_from) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
    )
    .bind(id.0)
    .bind(&c.kind)
    .bind(&c.content)
    .bind(&p.normalized)
    .bind(&p.topic)
    .bind(&p.draft.reason)
    .bind(&c.project)
    .bind(c.inferred)
    .bind(p.draft.observed_at)
    .bind(p.draft.valid_from)
    .bind(p.draft.valid_to)
    .bind(&p.draft.extraction_version)
    .bind(&p.dedupe_key)
    .bind(!row.review_reasons.is_empty())
    .bind(row.review_reasons)
    .bind(&memories)
    .bind(&candidates)
    .bind(row.edited_from.map(|e| e.0))
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    for ev in &c.evidence {
        sqlx::query("INSERT INTO memory_candidate_evidence (candidate_id, source_id, span) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(id.0)
            .bind(ev.source.0)
            .bind(&ev.span)
            .execute(&mut *conn)
            .await
            .map_err(db_err)?;
    }
    Ok(id)
}

impl PgMemory {
    /// Full proposal pipeline. Auto-accepts only under `policy::assess`; otherwise the candidate
    /// waits in the inbox with its review reasons and contradiction links.
    pub async fn propose_with_outcome(&self, draft: CandidateDraft) -> Result<Proposal> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let prepared = prepare(&mut tx, draft, None).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&prepared.dedupe_key)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let existing: Option<Uuid> = sqlx::query_scalar(
            "UPDATE memory_candidates SET seen_count = seen_count + 1 WHERE dedupe_key = $1 RETURNING id",
        )
        .bind(&prepared.dedupe_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if let Some(id) = existing {
            let proposal = proposal_from_row(&mut tx, CandidateId(id), true).await?;
            tx.commit().await.map_err(db_err)?;
            tracing::info!(candidate = %id, "duplicate candidate collapsed");
            return Ok(proposal);
        }

        let c = &prepared.draft.candidate;
        let (mem_links, cand_links) = match &prepared.topic {
            Some(topic) => (
                find_memory_contradictions(
                    &mut tx,
                    &c.kind,
                    c.project.as_deref(),
                    topic,
                    &prepared.normalized,
                    (self.clock)(),
                )
                .await?,
                find_candidate_contradictions(
                    &mut tx,
                    &c.kind,
                    c.project.as_deref(),
                    topic,
                    &prepared.normalized,
                    None,
                )
                .await?,
            ),
            None => (Vec::new(), Vec::new()),
        };
        let contradiction = !mem_links.is_empty() || !cand_links.is_empty();
        let assessment = policy::assess(
            &c.kind,
            c.inferred,
            &c.content,
            &prepared.facts,
            contradiction,
        );
        let id = insert_candidate(
            &mut tx,
            NewCandidateRow {
                prepared: &prepared,
                review_reasons: &assessment.review_reasons,
                contradicts_memories: &mem_links,
                contradicts_candidates: &cand_links,
                edited_from: None,
            },
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        tracing::info!(candidate = %id, auto_accept = assessment.auto_accept, contradiction, "candidate proposed");

        let mut auto_accepted = None;
        if assessment.auto_accept {
            match self
                .accept_with(id, AUTO_ACCEPT_ACTOR, AcceptMode::default())
                .await
            {
                Ok(memory) => auto_accepted = Some(memory),
                Err(err) => {
                    tracing::warn!(candidate = %id, error = %err, "auto-accept failed; candidate left pending for review")
                }
            }
        }
        Ok(Proposal {
            id,
            duplicate: false,
            needs_review: auto_accepted.is_none(),
            review_reasons: assessment.review_reasons,
            auto_accepted,
            contradicts_memories: mem_links,
            contradicts_candidates: cand_links,
        })
    }

    pub async fn propose_draft(&self, draft: CandidateDraft) -> Result<CandidateId> {
        Ok(self.propose_with_outcome(draft).await?.id)
    }

    /// Pending candidates, oldest first.
    pub async fn list_inbox(&self) -> Result<Vec<InboxEntry>> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM memory_candidates WHERE state = 'pending' ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut entries = Vec::with_capacity(ids.len());
        for id in ids {
            entries.push(self.get_candidate(CandidateId(id)).await?);
        }
        Ok(entries)
    }

    pub async fn get_candidate(&self, id: CandidateId) -> Result<InboxEntry> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        let row = sqlx::query(
            "SELECT kind, content, project, inferred, reason, state, review_reasons, needs_review, seen_count, \
             contradicts_memories, contradicts_candidates, edited_from, replaced_by, accepted_memory_id, created_at \
             FROM memory_candidates WHERE id = $1",
        )
        .bind(id.0)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("candidate", id))?;

        let evidence = sqlx::query(
            "SELECT e.source_id, e.span, s.kind, s.external_id, s.trust FROM memory_candidate_evidence e \
             JOIN sources s ON s.id = e.source_id WHERE e.candidate_id = $1 ORDER BY e.id",
        )
        .bind(id.0)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?
        .iter()
        .map(|r| {
            Ok(InboxEvidence {
                source: SourceId(r.try_get("source_id").map_err(db_err)?),
                source_kind: r.try_get("kind").map_err(db_err)?,
                external_id: r.try_get("external_id").map_err(db_err)?,
                trust: parse_trust(&r.try_get::<String, _>("trust").map_err(db_err)?)?,
                span: r.try_get("span").map_err(db_err)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

        let affected_ids: Vec<Uuid> = row.try_get("contradicts_memories").map_err(db_err)?;
        let affected = sqlx::query(
            "SELECT id, kind, content, status FROM memories WHERE id = ANY($1) ORDER BY id",
        )
        .bind(&affected_ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?
        .iter()
        .map(|r| {
            Ok(AffectedMemory {
                memory: MemoryId(r.try_get("id").map_err(db_err)?),
                kind: r.try_get("kind").map_err(db_err)?,
                content: r.try_get("content").map_err(db_err)?,
                status: r.try_get("status").map_err(db_err)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

        let replaced_by: Option<Uuid> = row.try_get("replaced_by").map_err(db_err)?;
        let mut active = replaced_by;
        while let Some(current) = active {
            let next: Option<Uuid> =
                sqlx::query_scalar("SELECT replaced_by FROM memory_candidates WHERE id = $1")
                    .bind(current)
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(db_err)?;
            if next.is_none() {
                break;
            }
            active = next;
        }
        let cand_links: Vec<Uuid> = row.try_get("contradicts_candidates").map_err(db_err)?;
        let edited_from: Option<Uuid> = row.try_get("edited_from").map_err(db_err)?;
        let accepted: Option<Uuid> = row.try_get("accepted_memory_id").map_err(db_err)?;
        Ok(InboxEntry {
            id,
            state: row.try_get("state").map_err(db_err)?,
            kind: row.try_get("kind").map_err(db_err)?,
            content: row.try_get("content").map_err(db_err)?,
            project: row.try_get("project").map_err(db_err)?,
            inferred: row.try_get("inferred").map_err(db_err)?,
            reason: row.try_get("reason").map_err(db_err)?,
            review_reasons: row.try_get("review_reasons").map_err(db_err)?,
            needs_review: row.try_get("needs_review").map_err(db_err)?,
            seen_count: row.try_get("seen_count").map_err(db_err)?,
            evidence,
            contradicts_memories: affected,
            contradicts_candidates: cand_links.into_iter().map(CandidateId).collect(),
            edited_from: edited_from.map(CandidateId),
            replaced_by: replaced_by.map(CandidateId),
            active_replacement: active.map(CandidateId),
            accepted_memory: accepted.map(MemoryId),
            created_at: row.try_get("created_at").map_err(db_err)?,
        })
    }
}

async fn proposal_from_row(
    conn: &mut PgConnection,
    id: CandidateId,
    duplicate: bool,
) -> Result<Proposal> {
    let row = sqlx::query(
        "SELECT needs_review, review_reasons, contradicts_memories, contradicts_candidates, accepted_memory_id, auto_accepted \
         FROM memory_candidates WHERE id = $1",
    )
    .bind(id.0)
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    let mems: Vec<Uuid> = row.try_get("contradicts_memories").map_err(db_err)?;
    let cands: Vec<Uuid> = row.try_get("contradicts_candidates").map_err(db_err)?;
    let accepted: Option<Uuid> = row.try_get("accepted_memory_id").map_err(db_err)?;
    Ok(Proposal {
        id,
        duplicate,
        needs_review: row.try_get("needs_review").map_err(db_err)?,
        review_reasons: row.try_get("review_reasons").map_err(db_err)?,
        auto_accepted: if row.try_get::<bool, _>("auto_accepted").map_err(db_err)? {
            accepted.map(MemoryId)
        } else {
            None
        },
        contradicts_memories: mems.into_iter().map(MemoryId).collect(),
        contradicts_candidates: cands.into_iter().map(CandidateId).collect(),
    })
}
