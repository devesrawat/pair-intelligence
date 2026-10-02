//! Evidence persistence (migrations/040_research_evidence.sql).
use super::types::{Claim, ResearchScope, Source};
use pair_core::error::{ErrorCode, PairError, Result};
use sqlx::PgPool;
use uuid::Uuid;

fn db(e: sqlx::Error) -> PairError {
    PairError::new(ErrorCode::Internal, format!("evidence store: {e}"))
}

#[derive(Clone)]
pub struct EvidenceStore {
    pool: PgPool,
}

impl EvidenceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create_run(&self, scope: &ResearchScope) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let json = serde_json::to_value(scope).map_err(|e| PairError::new(ErrorCode::Internal, e.to_string()))?;
        sqlx::query("INSERT INTO research_runs (id, question, scope) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(&scope.question)
            .bind(json)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(id)
    }

    pub async fn finish_run(&self, run: Uuid, ok: bool) -> Result<()> {
        let status = if ok { "completed" } else { "failed" };
        sqlx::query("UPDATE research_runs SET status = $2 WHERE id = $1").bind(run).bind(status).execute(&self.pool).await.map_err(db)?;
        Ok(())
    }

    /// Duplicates must be saved after the source they point at.
    pub async fn save_source(&self, run: Uuid, s: &Source) -> Result<()> {
        sqlx::query(
            "INSERT INTO research_sources (id, run_id, url, normalized_url, available, unavailable_reason, revision, \
             content_sha256, published_at, fetched_at, text, duplicate_of) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .bind(s.id.0)
        .bind(run)
        .bind(&s.url)
        .bind(&s.normalized_url)
        .bind(s.available)
        .bind(&s.unavailable_reason)
        .bind(&s.revision)
        .bind(&s.content_sha256)
        .bind(s.published_at)
        .bind(s.fetched_at)
        .bind(&s.text)
        .bind(s.duplicate_of.map(|d| d.0))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Stores the claim and its exact supporting span (pinned to the source version).
    pub async fn save_claim(&self, run: Uuid, c: &Claim, sources: &[Source]) -> Result<()> {
        let status = if c.is_valid() { "validated" } else { "rejected" };
        let reason = c.rejected.as_ref().map(ToString::to_string);
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "INSERT INTO research_claims (id, run_id, topic, claim_text, claim_value, status, reject_reason) \
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(c.id)
        .bind(run)
        .bind(&c.raw.topic)
        .bind(&c.raw.text)
        .bind(&c.raw.value)
        .bind(status)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let src = c.source.and_then(|id| sources.iter().find(|s| s.id == id));
        sqlx::query(
            "INSERT INTO research_claim_evidence (id, claim_id, source_id, cited_url, span, span_start, source_sha256, \
             source_published_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(Uuid::now_v7())
        .bind(c.id)
        .bind(src.map(|s| s.id.0))
        .bind(&c.raw.url)
        .bind(&c.raw.span)
        .bind(c.span_start.and_then(|v| i32::try_from(v).ok()))
        .bind(src.and_then(|s| s.content_sha256.clone()))
        .bind(src.and_then(|s| s.published_at))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    pub async fn save_report(&self, run: Uuid, markdown: &str) -> Result<()> {
        sqlx::query("INSERT INTO research_reports (run_id, markdown) VALUES ($1, $2)")
            .bind(run)
            .bind(markdown)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
