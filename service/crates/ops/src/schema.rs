//! Schema introspection: lets optional tables (for example `tool_executions`) be used when present.
use crate::error::Result;
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

/// Columns of the ordinary table that the unqualified name `table` resolves to through the
/// session's `search_path` (exactly what the purge SQL hits), in ordinal order. Empty when no
/// such table is visible. `to_regclass` + `pg_attribute` rather than `information_schema` with
/// `current_schema()`, which can name a different schema than the one the SQL resolves to.
pub(crate) async fn columns(pool: &PgPool, table: &str) -> Result<Vec<ColumnInfo>> {
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT a.attname::text, format_type(a.atttypid, NULL)::text, NOT a.attnotnull \
         FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
         WHERE a.attrelid = to_regclass($1) AND c.relkind IN ('r', 'p') \
           AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
    )
    .bind(table)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, data_type, nullable)| ColumnInfo {
            name,
            data_type,
            nullable,
        })
        .collect())
}

pub(crate) fn has_all(cols: &[ColumnInfo], required: &[&str]) -> bool {
    required.iter().all(|r| cols.iter().any(|c| c.name == *r))
}
