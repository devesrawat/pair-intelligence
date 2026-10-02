//! Memory store: candidate persistence, acceptance with provenance, supersession, expiry.
use crate::{
    audit,
    error::{db_err, not_found},
    model::{CandidateDraft, MemoryRecord},
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
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub const MAX_CONTENT_CHARS: usize = 4000;

#[derive(Clone)]
pub struct PgMemory {
    pub(crate) pool: PgPool,
    pub(crate) clock: fn() -> DateTime<Utc>,
}

/// How an acceptance should treat existing memories.
#[derive(Debug, Clone, Copy, Default)]
pub struct AcceptMode {
    pub supersedes: Option<MemoryId>,
    /// Resolve a contradiction by deliberately keeping both memories (recorded in `memory_conflicts`).
    pub keep_both: bool,
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

    pub async fn accept_superseding(&self, id: CandidateId, actor: &str, old: MemoryId) -> Result<MemoryId> {
        self.accept_with(id, actor, AcceptMode { supersedes: Some(old), keep_both: false }).await
    }

    pub async fn accept_with(&self, id: CandidateId, actor: &str, mode: AcceptMode) -> Result<MemoryId> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let memory = crate::accept::accept_in_tx(&mut tx, self.clock, id, actor, mode).await?;
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
        Ok(self.propose_with_outcome(CandidateDraft::new(c)).await?.id)
    }

    async fn accept(&self, id: CandidateId, actor: &str) -> Result<MemoryId> {
        self.accept_with(id, actor, AcceptMode::default()).await
    }

    async fn retrieve(&self, _q: RetrievalQuery) -> Result<Vec<EvidenceItem>> {
        Err(PairError::new(ErrorCode::Internal, "retrieval is implemented in Task 8"))
    }
}
