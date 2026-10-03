//! Contradiction detection between a statement and accepted memories / pending candidates, and
//! the per-topic lock that serialises it against concurrent proposals and acceptances.
use crate::error::db_err;
use chrono::{DateTime, Utc};
use pair_core::{
    error::Result,
    ids::{CandidateId, MemoryId},
};
use sqlx::PgConnection;
use uuid::Uuid;

/// Serialise contradiction check + write for one (kind, project, topic). Without this, two
/// contradictory statements racing under READ COMMITTED each see no conflict and both commit.
/// Transaction-scoped: released at commit or rollback.
pub(crate) async fn lock_topic(
    conn: &mut PgConnection,
    kind: &str,
    project: Option<&str>,
    topic: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended($1 || '|' || coalesce($2, '') || '|' || $3, 0))",
    )
    .bind(kind)
    .bind(project)
    .bind(topic.unwrap_or(""))
    .execute(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Accepted, currently valid memories on the same topic with different content.
async fn topic_memories(
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

async fn topic_candidates(
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

/// What a statement contradicts.
#[derive(Debug, Default)]
pub(crate) struct Links {
    pub memories: Vec<MemoryId>,
    pub candidates: Vec<CandidateId>,
}

/// Take the topic lock, then find accepted memories the statement contradicts.
pub(crate) async fn find_memory_links(
    conn: &mut PgConnection,
    kind: &str,
    project: Option<&str>,
    topic: Option<&str>,
    normalized: &str,
    as_of: DateTime<Utc>,
) -> Result<Vec<MemoryId>> {
    lock_topic(conn, kind, project, topic).await?;
    match topic {
        Some(topic) => topic_memories(conn, kind, project, topic, normalized, as_of).await,
        None => Ok(Vec::new()),
    }
}

/// Take the topic lock, then find accepted memories and pending candidates the statement
/// contradicts. `exclude` is the candidate being edited, which never contradicts itself.
pub(crate) async fn find_links(
    conn: &mut PgConnection,
    kind: &str,
    project: Option<&str>,
    topic: Option<&str>,
    normalized: &str,
    as_of: DateTime<Utc>,
    exclude: Option<Uuid>,
) -> Result<Links> {
    let memories = find_memory_links(conn, kind, project, topic, normalized, as_of).await?;
    let candidates = match topic {
        Some(topic) => topic_candidates(conn, kind, project, topic, normalized, exclude).await?,
        None => Vec::new(),
    };
    Ok(Links {
        memories,
        candidates,
    })
}
