//! Export of the owner's data (spec section 11): conversations, memories with evidence, goals,
//! optionally configuration, and the billing ledger.
//!
//! Output is deterministic (stable ordering, no wall-clock values) JSON-lines, one file per table,
//! a Markdown index, and `manifest.json` listing row counts and a SHA-256 per file. All reads happen
//! in one repeatable-read transaction, so the files are mutually consistent.
//!
//! Deletion semantics: a memory whose evidence is entirely from deleted sources is exported as a
//! tombstone (no content, topic or project), and evidence rows of deleted sources carry no span,
//! uri or external id. Configuration values that look like secrets are replaced by `[redacted]`.
mod markdown;
pub mod redact;

use crate::error::{OpsError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::path::Path;

pub const FORMAT_VERSION: u32 = 1;
pub const MANIFEST_FILE: &str = "manifest.json";
pub const INDEX_FILE: &str = "index.md";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub rows: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub include_config: bool,
    pub files: Vec<FileEntry>,
}

/// One exported row: the parsed value (for the Markdown index) and its canonical JSON line.
pub(crate) struct Row {
    pub value: Value,
    line: String,
}

const CONVERSATIONS_SQL: &str = "SELECT to_jsonb(t)::text FROM conversations t ORDER BY t.id";
const MESSAGES_SQL: &str =
    "SELECT to_jsonb(t)::text FROM messages t ORDER BY t.conversation_id, t.seq, t.id";
const GOALS_SQL: &str = "SELECT to_jsonb(t)::text FROM goals t ORDER BY t.id";
const OPEN_LOOPS_SQL: &str = "SELECT to_jsonb(t)::text FROM open_loops t ORDER BY t.id";
const RESERVATIONS_SQL: &str = "SELECT to_jsonb(t)::text FROM budget_reservations t ORDER BY t.id";
const LEDGER_SQL: &str = "SELECT to_jsonb(t)::text FROM budget_ledger t ORDER BY t.id";

/// Tombstone rule: a memory is live only while some evidence comes from an active source.
const MEMORIES_SQL: &str = "\
SELECT to_jsonb(x)::text FROM ( \
  SELECT m.id, m.kind, m.status, (NOT l.live) AS tombstone, \
         CASE WHEN l.live THEN m.content END AS content, \
         CASE WHEN l.live THEN m.topic_key END AS topic_key, \
         CASE WHEN l.live THEN m.project END AS project, \
         m.valid_from, m.valid_to, m.observed_at, m.confidence, m.importance, \
         m.supersedes_id, m.invalidated_reason, m.accepted_by, m.created_at \
  FROM memories m \
  CROSS JOIN LATERAL (SELECT EXISTS ( \
      SELECT 1 FROM memory_evidence e JOIN sources s ON s.id = e.source_id \
      WHERE e.memory_id = m.id AND s.deletion_state = 'active') AS live) l \
) x ORDER BY x.id";

const EVIDENCE_SQL: &str = "\
SELECT to_jsonb(x)::text FROM ( \
  SELECT e.id, e.memory_id, e.source_id, e.extraction_version, e.span_verified, e.created_at, \
         (s.deletion_state <> 'active') AS source_deleted, s.kind AS source_kind, s.revision, \
         CASE WHEN s.deletion_state = 'active' THEN e.span END AS span, \
         CASE WHEN s.deletion_state = 'active' THEN s.external_id END AS external_id, \
         CASE WHEN s.deletion_state = 'active' THEN s.uri END AS uri \
  FROM memory_evidence e JOIN sources s ON s.id = e.source_id \
) x ORDER BY x.id";

const CONFIG_ACCOUNTS_SQL: &str = "SELECT to_jsonb(t) FROM integration_accounts t ORDER BY t.id";
const CONFIG_ROUTINES_SQL: &str = "SELECT to_jsonb(t) FROM scheduled_routines t ORDER BY t.name";

/// Writes the export into `out_dir` (created if missing) and returns the manifest.
pub async fn export_all(pool: &PgPool, out_dir: &Path, include_config: bool) -> Result<Manifest> {
    tokio::fs::create_dir_all(out_dir)
        .await
        .map_err(|e| OpsError::io(out_dir, e))?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let conversations = rows(&mut tx, CONVERSATIONS_SQL).await?;
    let messages = rows(&mut tx, MESSAGES_SQL).await?;
    let memories = rows(&mut tx, MEMORIES_SQL).await?;
    let evidence = rows(&mut tx, EVIDENCE_SQL).await?;
    let goals = rows(&mut tx, GOALS_SQL).await?;
    let open_loops = rows(&mut tx, OPEN_LOOPS_SQL).await?;
    let reservations = rows(&mut tx, RESERVATIONS_SQL).await?;
    let ledger = rows(&mut tx, LEDGER_SQL).await?;
    let config = if include_config {
        Some(config_rows(&mut tx).await?)
    } else {
        None
    };
    tx.rollback().await?;

    let mut files = Vec::new();
    let mut tables: Vec<(&str, &[Row])> = vec![
        ("conversations.jsonl", &conversations),
        ("messages.jsonl", &messages),
        ("memories.jsonl", &memories),
        ("memory_evidence.jsonl", &evidence),
        ("goals.jsonl", &goals),
        ("open_loops.jsonl", &open_loops),
        ("billing_reservations.jsonl", &reservations),
        ("billing_ledger.jsonl", &ledger),
    ];
    if let Some(c) = &config {
        tables.push(("config.jsonl", c));
    }
    for (name, table) in tables {
        files.push(write_jsonl(out_dir, name, table).await?);
    }

    let index = markdown::render_index(&markdown::IndexInput {
        files: &files,
        conversations: &conversations,
        memories: &memories,
        evidence: &evidence,
        goals: &goals,
        open_loops: &open_loops,
    });
    files.push(write_file(out_dir, INDEX_FILE, &index, 1).await?);

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        include_config,
        files,
    };
    let mut text = serde_json::to_string_pretty(&manifest)?;
    text.push('\n');
    write_text(out_dir, MANIFEST_FILE, &text).await?;
    tracing::info!(
        files = manifest.files.len(),
        include_config,
        "export written"
    );
    Ok(manifest)
}

async fn rows(tx: &mut Transaction<'_, Postgres>, sql: &str) -> Result<Vec<Row>> {
    let lines: Vec<String> = sqlx::query_scalar(sql).fetch_all(&mut **tx).await?;
    lines
        .into_iter()
        .map(|text| {
            let value: Value = serde_json::from_str(&text)?;
            Ok(Row {
                line: escape_line_separators(&text),
                value,
            })
        })
        .collect()
}

/// Configuration rows with secrets redacted. Keys are kept so the owner sees what is configured.
async fn config_rows(tx: &mut Transaction<'_, Postgres>) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    for (table, sql) in [
        ("integration_accounts", CONFIG_ACCOUNTS_SQL),
        ("scheduled_routines", CONFIG_ROUTINES_SQL),
    ] {
        let values: Vec<Value> = sqlx::query_scalar(sql).fetch_all(&mut **tx).await?;
        for v in values {
            let wrapped = serde_json::json!({ "table": table, "row": redact::redact(&v) });
            out.push(Row {
                line: escape_line_separators(&serde_json::to_string(&wrapped)?),
                value: wrapped,
            });
        }
    }
    Ok(out)
}

/// U+2028, U+2029 and U+0085 are valid raw inside JSON strings but split lines in some readers.
fn escape_line_separators(json: &str) -> String {
    json.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .replace('\u{85}', "\\u0085")
}

async fn write_jsonl(dir: &Path, name: &str, table: &[Row]) -> Result<FileEntry> {
    let mut text = String::new();
    for r in table {
        text.push_str(&r.line);
        text.push('\n');
    }
    write_file(dir, name, &text, table.len() as u64).await
}

async fn write_file(dir: &Path, name: &str, text: &str, rows: u64) -> Result<FileEntry> {
    write_text(dir, name, text).await?;
    let sha256 = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(FileEntry {
        name: name.to_owned(),
        rows,
        sha256,
    })
}

async fn write_text(dir: &Path, name: &str, text: &str) -> Result<()> {
    let path = dir.join(name);
    tokio::fs::write(&path, text)
        .await
        .map_err(|e| OpsError::io(&path, e))
}
