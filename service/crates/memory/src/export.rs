//! Markdown and JSON export of memories with evidence. PostgreSQL stays the only writable
//! source of truth; exports are read-only views for inspection and portability.
use crate::{
    error::db_err,
    model::MemoryRecord,
    read::load_memories,
    sources::SOURCE_DELETED_REASON,
    store::PgMemory,
};
use chrono::{DateTime, Utc};
use pair_core::error::{PairError, ErrorCode, Result};
use serde::Serialize;
use std::fmt::Write;
use uuid::Uuid;

pub const EXPORT_SCHEMA_VERSION: u32 = 1;
const REDACTED: &str = "[redacted: source deleted]";

#[derive(Debug, Clone, Serialize)]
pub struct MemoryExport {
    pub schema_version: u32,
    pub exported_at: DateTime<Utc>,
    pub memories: Vec<MemoryRecord>,
}

/// Remove content derived from deleted sources; keeps ids and lifecycle for audit.
fn redact(mut m: MemoryRecord) -> MemoryRecord {
    if m.invalidated_reason.as_deref() == Some(SOURCE_DELETED_REASON) {
        m.content = REDACTED.to_string();
    }
    for ev in &mut m.evidence {
        if ev.source_deleted {
            ev.span = None;
            ev.uri = None;
        }
    }
    m
}

impl PgMemory {
    pub async fn export(&self) -> Result<MemoryExport> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM memories ORDER BY id")
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?;
        let memories = load_memories(&mut conn, &ids).await?.into_iter().map(redact).collect();
        Ok(MemoryExport { schema_version: EXPORT_SCHEMA_VERSION, exported_at: (self.clock)(), memories })
    }

    pub async fn export_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self.export().await?)
            .map_err(|e| PairError::new(ErrorCode::Internal, format!("export serialisation failed: {e}")))
    }

    pub async fn export_markdown(&self) -> Result<String> {
        Ok(render_markdown(&self.export().await?))
    }
}

pub fn render_markdown(export: &MemoryExport) -> String {
    let mut out = String::new();
    // Writing to a String cannot fail; results are intentionally discarded.
    let _ = writeln!(out, "# PAIR memory export\n\nExported {} (schema v{})\n", export.exported_at.to_rfc3339(), export.schema_version);
    for m in &export.memories {
        let _ = writeln!(out, "## {} [{}] {}", m.kind, m.status.as_str(), m.id);
        let _ = writeln!(out, "\n{}\n", m.content);
        let valid_to = m.valid_to.map_or_else(|| "open".to_string(), |t| t.to_rfc3339());
        let _ = writeln!(
            out,
            "- observed: {}\n- valid: {} to {}\n- project: {}\n- confidence: {}",
            m.observed_at.to_rfc3339(),
            m.valid_from.to_rfc3339(),
            valid_to,
            m.project.as_deref().unwrap_or("(global)"),
            if m.inferred { "inferred" } else { "observed" },
        );
        if let Some(prev) = m.supersedes {
            let _ = writeln!(out, "- supersedes: {prev}");
        }
        if let Some(reason) = &m.invalidated_reason {
            let _ = writeln!(out, "- invalidated: {reason}");
        }
        let _ = writeln!(out, "\nEvidence:");
        for ev in &m.evidence {
            let _ = writeln!(
                out,
                "- {}:{} r{} {} span: {}",
                ev.source_kind,
                ev.external_id,
                ev.revision,
                ev.uri.as_deref().unwrap_or(""),
                ev.span.as_deref().unwrap_or("(none)"),
            );
        }
        out.push('\n');
    }
    out
}
