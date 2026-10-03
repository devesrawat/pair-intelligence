//! Schema introspection: lets optional tables (for example `tool_executions`) be used when present.
use crate::error::Result;
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

/// Columns of `table` in the current schema, in ordinal order. Empty when the table does not exist.
pub(crate) async fn columns(pool: &PgPool, table: &str) -> Result<Vec<ColumnInfo>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name::text, data_type::text, is_nullable::text \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1 \
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, data_type, nullable)| ColumnInfo {
            name,
            data_type,
            nullable: nullable == "YES",
        })
        .collect())
}

pub(crate) fn has_all(cols: &[ColumnInfo], required: &[&str]) -> bool {
    required.iter().all(|r| cols.iter().any(|c| c.name == *r))
}
