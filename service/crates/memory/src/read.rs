//! Read helpers shared by the store, export and retrieval.
use crate::{
    error::db_err,
    model::{EvidenceRecord, MemoryRecord, MemoryStatus},
};
use pair_core::{
    error::Result,
    ids::{MemoryId, SourceId},
};
use sqlx::{postgres::PgRow, PgConnection, Row};
use std::collections::HashMap;
use uuid::Uuid;

pub(crate) fn evidence_from_row(row: &PgRow) -> Result<EvidenceRecord> {
    Ok(EvidenceRecord {
        source: SourceId(row.try_get("source_id").map_err(db_err)?),
        source_kind: row.try_get("kind").map_err(db_err)?,
        external_id: row.try_get("external_id").map_err(db_err)?,
        revision: row.try_get("revision").map_err(db_err)?,
        uri: row.try_get("uri").map_err(db_err)?,
        span: row.try_get("span").map_err(db_err)?,
        extraction_version: row.try_get("extraction_version").map_err(db_err)?,
        source_deleted: row.try_get::<String, _>("deletion_state").map_err(db_err)? == "deleted",
        source_hidden: row.try_get::<String, _>("visibility").map_err(db_err)? == "hidden",
    })
}

/// Load memories (with all evidence, including from deleted/hidden sources) ordered by id.
pub(crate) async fn load_memories(conn: &mut PgConnection, ids: &[Uuid]) -> Result<Vec<MemoryRecord>> {
    let rows = sqlx::query(
        "SELECT id, kind, status, content, topic_key, project, valid_from, valid_to, observed_at, \
                confidence, importance, supersedes_id, invalidated_reason, accepted_by \
         FROM memories WHERE id = ANY($1) ORDER BY id",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;

    let ev_rows = sqlx::query(
        "SELECT e.memory_id, e.source_id, e.span, e.extraction_version, \
                s.kind, s.external_id, s.revision, s.uri, s.deletion_state, s.visibility \
         FROM memory_evidence e JOIN sources s ON s.id = e.source_id \
         WHERE e.memory_id = ANY($1) ORDER BY e.id",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;
    let mut evidence: HashMap<Uuid, Vec<EvidenceRecord>> = HashMap::new();
    for row in &ev_rows {
        let memory_id: Uuid = row.try_get("memory_id").map_err(db_err)?;
        evidence.entry(memory_id).or_default().push(evidence_from_row(row)?);
    }

    rows.iter()
        .map(|row| {
            let id: Uuid = row.try_get("id").map_err(db_err)?;
            let confidence: String = row.try_get("confidence").map_err(db_err)?;
            let supersedes: Option<Uuid> = row.try_get("supersedes_id").map_err(db_err)?;
            Ok(MemoryRecord {
                id: MemoryId(id),
                kind: row.try_get("kind").map_err(db_err)?,
                status: MemoryStatus::parse(&row.try_get::<String, _>("status").map_err(db_err)?)?,
                content: row.try_get("content").map_err(db_err)?,
                topic_key: row.try_get("topic_key").map_err(db_err)?,
                project: row.try_get("project").map_err(db_err)?,
                valid_from: row.try_get("valid_from").map_err(db_err)?,
                valid_to: row.try_get("valid_to").map_err(db_err)?,
                observed_at: row.try_get("observed_at").map_err(db_err)?,
                inferred: confidence == "inferred",
                importance: row.try_get("importance").map_err(db_err)?,
                supersedes: supersedes.map(MemoryId),
                invalidated_reason: row.try_get("invalidated_reason").map_err(db_err)?,
                accepted_by: row.try_get("accepted_by").map_err(db_err)?,
                evidence: evidence.remove(&id).unwrap_or_default(),
            })
        })
        .collect()
}
