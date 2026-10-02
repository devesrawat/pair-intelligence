//! Audit events for changes to accepted memories (spec section 7: schema).
use crate::error::db_err;
use pair_core::error::Result;
use sqlx::PgConnection;
use uuid::Uuid;

pub const POLICY_VERSION: &str = "memory-policy-v1";

pub async fn record(
    conn: &mut PgConnection,
    actor: &str,
    action: &str,
    subject_kind: &str,
    subject_id: &str,
    metadata: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_events (id, actor, action, subject_kind, subject_id, policy_version, metadata) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::now_v7())
    .bind(actor)
    .bind(action)
    .bind(subject_kind)
    .bind(subject_id)
    .bind(POLICY_VERSION)
    .bind(metadata)
    .execute(conn)
    .await
    .map_err(db_err)?;
    tracing::info!(actor, action, subject_kind, subject_id, "audit event recorded");
    Ok(())
}
