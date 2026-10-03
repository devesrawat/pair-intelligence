//! Markdown and JSON export of memories with evidence. PostgreSQL stays the only writable
//! source of truth; exports are read-only views for inspection and portability.
use crate::{error::db_err, model::MemoryRecord, read::load_memories, store::PgMemory};
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use serde::Serialize;
use std::fmt::Write;
use uuid::Uuid;

pub const EXPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct MemoryExport {
    pub schema_version: u32,
    pub exported_at: DateTime<Utc>,
    pub memories: Vec<MemoryRecord>,
}

impl PgMemory {
    pub async fn export(&self) -> Result<MemoryExport> {
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM memories ORDER BY id")
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?;
        let memories = load_memories(&mut conn, &ids).await?;
        Ok(MemoryExport {
            schema_version: EXPORT_SCHEMA_VERSION,
            exported_at: (self.clock)(),
            memories,
        })
    }

    pub async fn export_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self.export().await?).map_err(|e| {
            PairError::new(
                ErrorCode::Internal,
                format!("export serialisation failed: {e}"),
            )
        })
    }

    pub async fn export_markdown(&self) -> Result<String> {
        Ok(render_markdown(&self.export().await?))
    }
}

/// Characters a Markdown renderer or line-based parser may treat as a line break.
const LINE_BREAKS: [char; 5] = ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'];
const CODE_INDENT: &str = "    ";

/// Free text as an indented code block: every line is prefixed, so no stored line can start a
/// heading, list item or fence of its own.
fn indented_block(text: &str) -> String {
    let unified: String = text
        .chars()
        .map(|c| if LINE_BREAKS.contains(&c) { '\n' } else { c })
        .collect();
    unified
        .split('\n')
        .map(|line| format!("{CODE_INDENT}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Free text as a single-line code span: line breaks become a visible `\n`, and the delimiter is
/// longer than any backtick run inside, so the value cannot close the span or start a new line.
fn code_span(text: &str) -> String {
    let flat: String = text
        .chars()
        .flat_map(|c| {
            if LINE_BREAKS.contains(&c) {
                vec!['\\', 'n']
            } else {
                vec![c]
            }
        })
        .collect();
    let longest_run = flat
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest_run + 1);
    format!("{fence} {flat} {fence}")
}

pub fn render_markdown(export: &MemoryExport) -> String {
    let mut out = String::new();
    // Writing to a String cannot fail; results are intentionally discarded.
    let _ = writeln!(
        out,
        "# PAIR memory export\n\nExported {} (schema v{})\n",
        export.exported_at.to_rfc3339(),
        export.schema_version
    );
    for m in &export.memories {
        let _ = writeln!(out, "## {} [{}] {}", m.kind, m.status.as_str(), m.id);
        let _ = writeln!(out, "\n{}\n", indented_block(&m.content));
        let valid_to = m
            .valid_to
            .map_or_else(|| "open".to_string(), |t| t.to_rfc3339());
        let project = m
            .project
            .as_deref()
            .map_or_else(|| "(global)".to_string(), code_span);
        let _ = writeln!(
            out,
            "- observed: {}\n- valid: {} to {}\n- project: {}\n- confidence: {}",
            m.observed_at.to_rfc3339(),
            m.valid_from.to_rfc3339(),
            valid_to,
            project,
            if m.inferred { "inferred" } else { "observed" },
        );
        if let Some(prev) = m.supersedes {
            let _ = writeln!(out, "- supersedes: {prev}");
        }
        if let Some(reason) = &m.invalidated_reason {
            let _ = writeln!(out, "- invalidated: {}", code_span(reason));
        }
        let _ = writeln!(out, "\nEvidence:");
        for ev in &m.evidence {
            let _ = writeln!(
                out,
                "- source {}:{} r{} uri {} span{}: {}",
                code_span(&ev.source_kind),
                code_span(&ev.external_id),
                ev.revision,
                ev.uri
                    .as_deref()
                    .map_or_else(|| "(none)".to_string(), code_span),
                if ev.span_verified {
                    ""
                } else {
                    " (unverified)"
                },
                ev.span
                    .as_deref()
                    .map_or_else(|| "(none)".to_string(), code_span),
            );
        }
        out.push('\n');
    }
    out
}
