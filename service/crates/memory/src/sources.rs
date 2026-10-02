//! Source registry: revisions, visibility, and deletion with derived-memory invalidation.
use crate::{
    audit,
    error::{db_err, not_found},
    model::{
        data_class_str, parse_data_class, parse_trust, trust_str, NewSource, SourceRecord,
        Visibility,
    },
    store::PgMemory,
};
use pair_core::{
    error::Result,
    ids::{MemoryId, SourceId},
};
use sqlx::{postgres::PgRow, Row};
use uuid::Uuid;

pub const SOURCE_DELETED_REASON: &str = "source_deleted";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionReport {
    pub source_revisions: u64,
    pub invalidated: Vec<MemoryId>,
}

pub(crate) fn source_from_row(row: &PgRow) -> Result<SourceRecord> {
    Ok(SourceRecord {
        id: SourceId(row.try_get("id").map_err(db_err)?),
        kind: row.try_get("kind").map_err(db_err)?,
        external_id: row.try_get("external_id").map_err(db_err)?,
        revision: row.try_get("revision").map_err(db_err)?,
        content_hash: row.try_get("content_hash").map_err(db_err)?,
        captured_at: row.try_get("captured_at").map_err(db_err)?,
        data_class: parse_data_class(&row.try_get::<String, _>("data_class").map_err(db_err)?)?,
        trust: parse_trust(&row.try_get::<String, _>("trust").map_err(db_err)?)?,
        uri: row.try_get("uri").map_err(db_err)?,
        visibility: if row.try_get::<String, _>("visibility").map_err(db_err)? == "hidden" {
            Visibility::Hidden
        } else {
            Visibility::Visible
        },
        deleted: row.try_get::<String, _>("deletion_state").map_err(db_err)? == "deleted",
    })
}

const SOURCE_COLUMNS: &str =
    "id, kind, external_id, revision, content_hash, captured_at, data_class, trust, uri, visibility, deletion_state";

impl PgMemory {
    /// Register a source. An unchanged hash reuses the latest revision; a changed hash
    /// creates the next revision, inheriting deletion and visibility state of the identity.
    pub async fn register_source(&self, new: NewSource) -> Result<SourceRecord> {
        if new.kind.trim().is_empty()
            || new.external_id.trim().is_empty()
            || new.content_hash.trim().is_empty()
        {
            return Err(crate::error::invalid(
                "source kind, external_id and content_hash are required",
            ));
        }
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{}:{}", new.kind, new.external_id))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let latest = sqlx::query(&format!(
            "SELECT {SOURCE_COLUMNS}, deleted_at FROM sources WHERE kind = $1 AND external_id = $2 \
             ORDER BY revision DESC LIMIT 1"
        ))
        .bind(&new.kind)
        .bind(&new.external_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;

        let record = match latest {
            Some(row)
                if row.try_get::<String, _>("content_hash").map_err(db_err)?
                    == new.content_hash =>
            {
                source_from_row(&row)?
            }
            other => {
                let (revision, visibility, deletion, deleted_at) = match &other {
                    Some(row) => (
                        row.try_get::<i32, _>("revision").map_err(db_err)? + 1,
                        row.try_get::<String, _>("visibility").map_err(db_err)?,
                        row.try_get::<String, _>("deletion_state").map_err(db_err)?,
                        row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("deleted_at")
                            .map_err(db_err)?,
                    ),
                    None => (1, "visible".to_string(), "active".to_string(), None),
                };
                let row = sqlx::query(&format!(
                    "INSERT INTO sources (id, kind, external_id, revision, content_hash, captured_at, data_class, \
                     trust, uri, visibility, deletion_state, deleted_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING {SOURCE_COLUMNS}"
                ))
                .bind(Uuid::now_v7())
                .bind(&new.kind)
                .bind(&new.external_id)
                .bind(revision)
                .bind(&new.content_hash)
                .bind(new.captured_at.unwrap_or_else(|| (self.clock)()))
                .bind(data_class_str(new.data_class))
                .bind(trust_str(new.trust))
                .bind(&new.uri)
                .bind(visibility)
                .bind(deletion)
                .bind(deleted_at)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                source_from_row(&row)?
            }
        };
        tx.commit().await.map_err(db_err)?;
        tracing::info!(source = %record.id, revision = record.revision, "source registered");
        Ok(record)
    }

    pub async fn get_source(&self, id: SourceId) -> Result<SourceRecord> {
        let row = sqlx::query(&format!(
            "SELECT {SOURCE_COLUMNS} FROM sources WHERE id = $1"
        ))
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found("source", id))?;
        source_from_row(&row)
    }

    /// Change visibility for every revision of the source identity. Retrieval re-checks
    /// visibility at query time, so memories derived only from it disappear immediately.
    pub async fn set_source_visibility(
        &self,
        id: SourceId,
        visibility: Visibility,
        actor: &str,
    ) -> Result<u64> {
        let src = self.get_source(id).await?;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let changed =
            sqlx::query("UPDATE sources SET visibility = $3 WHERE kind = $1 AND external_id = $2")
                .bind(&src.kind)
                .bind(&src.external_id)
                .bind(visibility.as_str())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?
                .rows_affected();
        audit::record(
            &mut tx,
            actor,
            "source.visibility_changed",
            "source",
            &id.to_string(),
            serde_json::json!({ "visibility": visibility.as_str(), "revisions": changed }),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(changed)
    }

    /// Mark every revision of the source deleted. Memories left without any active evidence are
    /// invalidated (status `expired`, reason `source_deleted`) and their chunks removed.
    pub async fn delete_source(&self, id: SourceId, actor: &str) -> Result<DeletionReport> {
        let src = self.get_source(id).await?;
        let now = (self.clock)();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let revisions = sqlx::query(
            "UPDATE sources SET deletion_state = 'deleted', deleted_at = $3 \
             WHERE kind = $1 AND external_id = $2 AND deletion_state = 'active'",
        )
        .bind(&src.kind)
        .bind(&src.external_id)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();

        let doomed: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT m.id FROM memories m \
             JOIN memory_evidence e ON e.memory_id = m.id JOIN sources s ON s.id = e.source_id \
             WHERE s.kind = $1 AND s.external_id = $2 AND m.invalidated_reason IS NULL \
               AND NOT EXISTS (SELECT 1 FROM memory_evidence e2 JOIN sources s2 ON s2.id = e2.source_id \
                               WHERE e2.memory_id = m.id AND s2.deletion_state = 'active') \
             ORDER BY m.id",
        )
        .bind(&src.kind)
        .bind(&src.external_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;

        sqlx::query(
            "UPDATE memories SET status = 'expired', invalidated_reason = $2, \
                    valid_to = GREATEST(valid_from, LEAST(COALESCE(valid_to, $3), $3)) \
             WHERE id = ANY($1)",
        )
        .bind(&doomed)
        .bind(SOURCE_DELETED_REASON)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "DELETE FROM memory_chunks WHERE memory_id = ANY($1) \
             OR source_id IN (SELECT id FROM sources WHERE kind = $2 AND external_id = $3)",
        )
        .bind(&doomed)
        .bind(&src.kind)
        .bind(&src.external_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        for memory in &doomed {
            audit::record(
                &mut tx,
                actor,
                "memory.invalidated",
                "memory",
                &memory.to_string(),
                serde_json::json!({ "reason": SOURCE_DELETED_REASON, "source": id.to_string() }),
            )
            .await?;
        }
        audit::record(
            &mut tx,
            actor,
            "source.deleted",
            "source",
            &id.to_string(),
            serde_json::json!({ "revisions": revisions, "invalidated": doomed.len() }),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        tracing::info!(source = %id, invalidated = doomed.len(), "source deleted");
        Ok(DeletionReport {
            source_revisions: revisions,
            invalidated: doomed.into_iter().map(MemoryId).collect(),
        })
    }
}
