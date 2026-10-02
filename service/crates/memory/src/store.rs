//! Memory store: candidate persistence, acceptance with provenance, supersession, expiry.
use crate::{
    audit,
    error::{db_err, invalid, not_found},
    model::{validate_kind, CandidateDraft, MemoryRecord},
    normalize::{normalize_content, topic_of},
    read::load_memories,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{CandidateId, MemoryId},
    traits::Memory,
    types::{EvidenceRef, EvidenceItem, MemoryCandidate, RetrievalQuery},
};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

pub const MAX_CONTENT_CHARS: usize = 4000;
const DEFAULT_IMPORTANCE: i16 = 1;

#[derive(Clone)]
pub struct PgMemory {
    pub(crate) pool: PgPool,
    pub(crate) clock: fn() -> DateTime<Utc>,
}

/// How an acceptance should treat existing memories.
#[derive(Debug, Clone, Copy, Default)]
pub struct AcceptMode {
    pub supersedes: Option<MemoryId>,
}

impl PgMemory {
    pub fn new(pool: PgPool) -> Self {
        Self { pool, clock: Utc::now }
    }

    pub fn with_clock(mut self, clock: fn() -> DateTime<Utc>) -> Self {
        self.clock = clock;
        self
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Insert a pending candidate and its evidence. No deduplication; see `inbox` for the
    /// full proposal pipeline.
    pub async fn propose_draft(&self, draft: CandidateDraft) -> Result<CandidateId> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let id = insert_candidate(&mut tx, &draft).await?;
        tx.commit().await.map_err(db_err)?;
        tracing::info!(candidate = %id, kind = %draft.candidate.kind, "candidate proposed");
        Ok(id)
    }

    pub async fn accept_superseding(&self, id: CandidateId, actor: &str, old: MemoryId) -> Result<MemoryId> {
        self.accept_with(id, actor, AcceptMode { supersedes: Some(old) }).await
    }

    pub async fn accept_with(&self, id: CandidateId, actor: &str, mode: AcceptMode) -> Result<MemoryId> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let memory = accept_in_tx(&mut tx, self.clock, id, actor, mode).await?;
        tx.commit().await.map_err(db_err)?;
        tracing::info!(candidate = %id, memory = %memory, actor, "candidate accepted");
        Ok(memory)
    }

    pub async fn get_memory(&self, id: MemoryId) -> Result<MemoryRecord> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        load_memories(&mut conn, &[id.0]).await?.into_iter().next().ok_or_else(|| not_found("memory", id))
    }

    /// Full supersession chain containing `id`, oldest first.
    pub async fn memory_chain(&self, id: MemoryId) -> Result<Vec<MemoryRecord>> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        let mut head = load_memories(&mut conn, &[id.0]).await?.into_iter().next().ok_or_else(|| not_found("memory", id))?;
        let mut chain = vec![head.clone()];
        while let Some(prev) = head.supersedes {
            head = load_memories(&mut conn, &[prev.0]).await?.into_iter().next().ok_or_else(|| not_found("memory", prev))?;
            chain.push(head.clone());
        }
        chain.reverse();
        loop {
            let tail = chain.last().map(|m| m.id.0).unwrap_or(id.0);
            let next: Option<Uuid> = sqlx::query_scalar("SELECT id FROM memories WHERE supersedes_id = $1")
                .bind(tail)
                .fetch_optional(&mut *conn)
                .await
                .map_err(db_err)?;
            match next {
                Some(n) => chain.extend(load_memories(&mut conn, &[n]).await?),
                None => return Ok(chain),
            }
        }
    }

    /// Close validity of an accepted memory (lifecycle: accepted -> expired).
    pub async fn expire_memory(&self, id: MemoryId, actor: &str, reason: &str) -> Result<()> {
        let now = (self.clock)();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let updated = sqlx::query(
            "UPDATE memories SET status = 'expired', valid_to = GREATEST(valid_from, LEAST(COALESCE(valid_to, $2), $2)) \
             WHERE id = $1 AND status = 'accepted'",
        )
        .bind(id.0)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if updated == 0 {
            return Err(PairError::new(ErrorCode::Conflict, format!("memory {id} is not an accepted memory")));
        }
        audit::record(&mut tx, actor, "memory.expired", "memory", &id.to_string(), serde_json::json!({ "reason": reason })).await?;
        tx.commit().await.map_err(db_err)
    }
}

pub(crate) async fn insert_candidate(conn: &mut PgConnection, draft: &CandidateDraft) -> Result<CandidateId> {
    let c = &draft.candidate;
    validate_kind(&c.kind)?;
    let content = c.content.trim();
    if content.is_empty() || content.chars().count() > MAX_CONTENT_CHARS {
        return Err(invalid(format!("candidate content must be 1..={MAX_CONTENT_CHARS} characters")));
    }
    let id = CandidateId::new();
    let topic = draft.topic.as_deref().map(normalize_content).or_else(|| topic_of(content));
    sqlx::query(
        "INSERT INTO memory_candidates (id, kind, content, normalized_content, topic_key, reason, project, inferred, \
         observed_at, valid_from, valid_to, extraction_version) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id.0)
    .bind(&c.kind)
    .bind(content)
    .bind(normalize_content(content))
    .bind(topic)
    .bind(&draft.reason)
    .bind(&c.project)
    .bind(c.inferred)
    .bind(draft.observed_at)
    .bind(draft.valid_from)
    .bind(draft.valid_to)
    .bind(&draft.extraction_version)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    for ev in &c.evidence {
        sqlx::query(
            "INSERT INTO memory_candidate_evidence (candidate_id, source_id, span) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(id.0)
        .bind(ev.source.0)
        .bind(&ev.span)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    }
    Ok(id)
}

struct CandidateRow {
    kind: String,
    content: String,
    normalized: String,
    topic: Option<String>,
    project: Option<String>,
    inferred: bool,
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
        "SELECT kind, content, normalized_content, topic_key, project, inferred, state, observed_at, valid_from, \
         valid_to, extraction_version FROM memory_candidates WHERE id = $1 FOR UPDATE",
    )
    .bind(id.0)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)?
    .ok_or_else(|| not_found("candidate", id))?;
    let state: String = row.try_get("state").map_err(db_err)?;
    if state != "pending" {
        return Err(PairError::new(ErrorCode::Conflict, format!("candidate {id} is {state}, not pending")));
    }
    let cand = CandidateRow {
        kind: row.try_get("kind").map_err(db_err)?,
        content: row.try_get("content").map_err(db_err)?,
        normalized: row.try_get("normalized_content").map_err(db_err)?,
        topic: row.try_get("topic_key").map_err(db_err)?,
        project: row.try_get("project").map_err(db_err)?,
        inferred: row.try_get("inferred").map_err(db_err)?,
        observed_at: row.try_get("observed_at").map_err(db_err)?,
        valid_from: row.try_get("valid_from").map_err(db_err)?,
        valid_to: row.try_get("valid_to").map_err(db_err)?,
        extraction_version: row.try_get("extraction_version").map_err(db_err)?,
    };

    let evidence = sqlx::query(
        "SELECT e.source_id, e.span, s.deletion_state FROM memory_candidate_evidence e \
         JOIN sources s ON s.id = e.source_id WHERE e.candidate_id = $1 ORDER BY e.id",
    )
    .bind(id.0)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;
    if evidence.is_empty() {
        return Err(PairError::new(ErrorCode::MemoryNoEvidence, "an accepted memory requires at least one evidence reference"));
    }
    let mut live: Vec<EvidenceRef> = Vec::with_capacity(evidence.len());
    for ev in &evidence {
        if ev.try_get::<String, _>("deletion_state").map_err(db_err)? == "active" {
            live.push(EvidenceRef {
                source: pair_core::ids::SourceId(ev.try_get("source_id").map_err(db_err)?),
                span: ev.try_get("span").map_err(db_err)?,
            });
        }
    }
    if live.is_empty() {
        return Err(PairError::new(ErrorCode::SourceDeleted, "all evidence sources of this candidate are deleted"));
    }

    let now = clock();
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
    .bind(DEFAULT_IMPORTANCE)
    .bind(mode.supersedes.map(|m| m.0))
    .bind(actor)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    insert_evidence_and_chunks(conn, memory_id, &cand.content, &live, &cand.extraction_version).await?;

    sqlx::query(
        "UPDATE memory_candidates SET state = 'accepted', accepted_memory_id = $2, reviewed_by = $3, reviewed_at = $4 WHERE id = $1",
    )
    .bind(id.0)
    .bind(memory_id.0)
    .bind(actor)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    audit::record(
        conn,
        actor,
        "memory.accepted",
        "memory",
        &memory_id.to_string(),
        serde_json::json!({ "candidate": id.to_string(), "kind": cand.kind, "inferred": cand.inferred }),
    )
    .await?;
    Ok(memory_id)
}

async fn close_superseded(conn: &mut PgConnection, old: MemoryId, new_valid_from: DateTime<Utc>, actor: &str) -> Result<()> {
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
        return Err(PairError::new(ErrorCode::Conflict, format!("memory {old} cannot be superseded (not an active accepted memory)")));
    }
    audit::record(conn, actor, "memory.superseded", "memory", &old.to_string(), serde_json::json!({})).await
}

/// Persist evidence rows and search chunks (memory text plus each distinct evidence span).
pub(crate) async fn insert_evidence_and_chunks(
    conn: &mut PgConnection,
    memory: MemoryId,
    content: &str,
    evidence: &[EvidenceRef],
    extraction_version: &str,
) -> Result<()> {
    insert_chunk(conn, memory, None, content).await?;
    for ev in evidence {
        sqlx::query(
            "INSERT INTO memory_evidence (memory_id, source_id, span, extraction_version) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING",
        )
        .bind(memory.0)
        .bind(ev.source.0)
        .bind(&ev.span)
        .bind(extraction_version)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
        if let Some(span) = ev.span.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            insert_chunk(conn, memory, Some(ev.source.0), span).await?;
        }
    }
    Ok(())
}

async fn insert_chunk(conn: &mut PgConnection, memory: MemoryId, source: Option<Uuid>, text: &str) -> Result<()> {
    sqlx::query("INSERT INTO memory_chunks (id, memory_id, source_id, text) VALUES ($1, $2, $3, $4)")
        .bind(Uuid::now_v7())
        .bind(memory.0)
        .bind(source)
        .bind(text)
        .execute(conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

#[async_trait]
impl Memory for PgMemory {
    async fn propose(&self, c: MemoryCandidate) -> Result<CandidateId> {
        self.propose_draft(CandidateDraft::new(c)).await
    }

    async fn accept(&self, id: CandidateId, actor: &str) -> Result<MemoryId> {
        self.accept_with(id, actor, AcceptMode::default()).await
    }

    async fn retrieve(&self, _q: RetrievalQuery) -> Result<Vec<EvidenceItem>> {
        Err(PairError::new(ErrorCode::Internal, "retrieval is implemented in Task 8"))
    }
}
