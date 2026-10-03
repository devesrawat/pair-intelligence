//! Contradiction detection between a statement and accepted memories / pending candidates, and
//! the per-topic lock that serialises it against concurrent proposals and acceptances.
use crate::error::db_err;
use chrono::{DateTime, Utc};
use pair_core::{
    error::Result,
    ids::{CandidateId, MemoryId},
};
use sqlx::PgConnection;
use std::collections::HashSet;
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

/// Kinds whose contradictions are also detected by wording similarity, independent of any topic.
/// Preferences are the only kind that can auto-accept, and the heuristic topic misses phrasings
/// like "I prefer dark mode"; an extractor-chosen topic must not be able to hide a conflict either.
const SIMILARITY_KINDS: [&str; 1] = ["preference"];
/// Token Jaccard overlap at or above which two different statements are treated as competing.
const SIMILARITY_THRESHOLD: f64 = 0.5;
/// Most recent rows scanned per similarity check.
const SIMILARITY_SCAN_LIMIT: i64 = 500;

/// Jaccard overlap of the token sets of two normalised statements.
pub(crate) fn token_similarity(a: &str, b: &str) -> f64 {
    let sa: HashSet<&str> = a.split(' ').filter(|t| !t.is_empty()).collect();
    let sb: HashSet<&str> = b.split(' ').filter(|t| !t.is_empty()).collect();
    let union = sa.union(&sb).count();
    if union == 0 {
        return 0.0;
    }
    sa.intersection(&sb).count() as f64 / union as f64
}

fn is_similar(a: &str, b: &str) -> bool {
    a != b && token_similarity(a, b) >= SIMILARITY_THRESHOLD
}

async fn similar_memories(
    conn: &mut PgConnection,
    kind: &str,
    project: Option<&str>,
    normalized: &str,
    as_of: DateTime<Utc>,
) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, normalized_content FROM memories WHERE status = 'accepted' \
         AND invalidated_reason IS NULL AND kind = $1 AND project IS NOT DISTINCT FROM $2 \
         AND normalized_content <> $3 AND (valid_to IS NULL OR valid_to > $4) \
         ORDER BY id DESC LIMIT $5",
    )
    .bind(kind)
    .bind(project)
    .bind(normalized)
    .bind(as_of)
    .bind(SIMILARITY_SCAN_LIMIT)
    .fetch_all(conn)
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .filter(|(_, n)| is_similar(normalized, n))
        .map(|(id, _)| id)
        .collect())
}

async fn similar_candidates(
    conn: &mut PgConnection,
    kind: &str,
    project: Option<&str>,
    normalized: &str,
    exclude: Option<Uuid>,
) -> Result<Vec<Uuid>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, normalized_content FROM memory_candidates WHERE state = 'pending' \
         AND kind = $1 AND project IS NOT DISTINCT FROM $2 AND normalized_content <> $3 \
         AND id IS DISTINCT FROM $4 ORDER BY id DESC LIMIT $5",
    )
    .bind(kind)
    .bind(project)
    .bind(normalized)
    .bind(exclude)
    .bind(SIMILARITY_SCAN_LIMIT)
    .fetch_all(conn)
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .filter(|(_, n)| is_similar(normalized, n))
        .map(|(id, _)| id)
        .collect())
}

/// Sorted union without duplicates.
fn merge_ids(mut a: Vec<Uuid>, b: Vec<Uuid>) -> Vec<Uuid> {
    a.extend(b);
    a.sort();
    a.dedup();
    a
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
    let by_topic = match topic {
        Some(topic) => topic_memories(conn, kind, project, topic, normalized, as_of)
            .await?
            .into_iter()
            .map(|m| m.0)
            .collect(),
        None => Vec::new(),
    };
    let by_wording = if SIMILARITY_KINDS.contains(&kind) {
        similar_memories(conn, kind, project, normalized, as_of).await?
    } else {
        Vec::new()
    };
    Ok(merge_ids(by_topic, by_wording)
        .into_iter()
        .map(MemoryId)
        .collect())
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
    let by_topic = match topic {
        Some(topic) => topic_candidates(conn, kind, project, topic, normalized, exclude)
            .await?
            .into_iter()
            .map(|c| c.0)
            .collect(),
        None => Vec::new(),
    };
    let by_wording = if SIMILARITY_KINDS.contains(&kind) {
        similar_candidates(conn, kind, project, normalized, exclude).await?
    } else {
        Vec::new()
    };
    Ok(Links {
        memories,
        candidates: merge_ids(by_topic, by_wording)
            .into_iter()
            .map(CandidateId)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_similarity_dark_vs_light_mode_is_competing() {
        assert!(is_similar("i prefer dark mode", "i prefer light mode"));
    }

    #[test]
    fn test_token_similarity_unrelated_statements_not_competing() {
        assert!(!is_similar("i prefer dark mode", "i prefer vim bindings"));
    }

    #[test]
    fn test_token_similarity_identical_statements_not_a_contradiction() {
        assert!(!is_similar("i prefer dark mode", "i prefer dark mode"));
    }
}
