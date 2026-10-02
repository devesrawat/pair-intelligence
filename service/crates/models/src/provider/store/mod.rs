//! Conversation persistence (Postgres). Everything here survives a service restart.
mod calls;
mod reconcile;

pub use calls::{CallStatus, CostState, ModelCallRecord};

use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TraceId};
use pair_core::types::TrustClass;
use pair_telemetry::redact_secrets;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub(crate) fn db_err(e: &sqlx::Error) -> PairError {
    PairError::new(
        ErrorCode::Internal,
        redact_secrets(&format!("database error: {e}")),
    )
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub conversation: ConversationId,
    /// Unique per conversation; re-appending the same id is a no-op that returns the original.
    pub client_message_id: String,
    pub role: String,
    pub content: String,
    pub trust: TrustClass,
    pub trace: TraceId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub id: Uuid,
    pub conversation: ConversationId,
    pub client_message_id: String,
    pub seq: i32,
    pub role: String,
    pub content: String,
    pub trust: TrustClass,
    pub trace: TraceId,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ConversationStore {
    pool: PgPool,
}

impl ConversationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn create_conversation(&self, title: &str, trace: TraceId) -> Result<ConversationId> {
        let id = ConversationId::new();
        sqlx::query("INSERT INTO conversations (id, title, trace_id) VALUES ($1, $2, $3)")
            .bind(id.0)
            .bind(title)
            .bind(trace.0)
            .execute(&self.pool)
            .await
            .map_err(|e| db_err(&e))?;
        Ok(id)
    }

    /// Idempotent append. Returns the stored message and whether it was newly inserted.
    pub async fn append_message(&self, m: NewMessage) -> Result<(StoredMessage, bool)> {
        let mut tx = self.pool.begin().await.map_err(|e| db_err(&e))?;
        let locked = sqlx::query("SELECT id FROM conversations WHERE id = $1 FOR UPDATE")
            .bind(m.conversation.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| db_err(&e))?;
        if locked.is_none() {
            return Err(PairError::new(
                ErrorCode::NotFound,
                format!("conversation {} not found", m.conversation),
            ));
        }
        let existing = sqlx::query(SELECT_BY_CLIENT_ID)
            .bind(m.conversation.0)
            .bind(&m.client_message_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| db_err(&e))?;
        if let Some(row) = existing {
            return Ok((row_to_message(&row)?, false));
        }
        let row = sqlx::query(INSERT_MESSAGE)
            .bind(Uuid::now_v7())
            .bind(m.conversation.0)
            .bind(&m.client_message_id)
            .bind(&m.role)
            .bind(&m.content)
            .bind(trust_str(m.trust))
            .bind(m.trace.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| db_err(&e))?;
        sqlx::query("UPDATE conversations SET updated_at = now() WHERE id = $1")
            .bind(m.conversation.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_err(&e))?;
        tx.commit().await.map_err(|e| db_err(&e))?;
        Ok((row_to_message(&row)?, true))
    }

    pub async fn list_messages(&self, conversation: ConversationId) -> Result<Vec<StoredMessage>> {
        let rows = sqlx::query(&format!(
            "{SELECT_COLUMNS} WHERE conversation_id = $1 ORDER BY seq"
        ))
        .bind(conversation.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| db_err(&e))?;
        rows.iter().map(row_to_message).collect()
    }
}

const SELECT_COLUMNS: &str = "SELECT id, conversation_id, client_message_id, seq, role, content, trust, trace_id, created_at FROM messages";
const SELECT_BY_CLIENT_ID: &str = "SELECT id, conversation_id, client_message_id, seq, role, content, trust, trace_id, created_at \
     FROM messages WHERE conversation_id = $1 AND client_message_id = $2";
const INSERT_MESSAGE: &str = "INSERT INTO messages (id, conversation_id, client_message_id, seq, role, content, trust, trace_id) \
     VALUES ($1, $2, $3, (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE conversation_id = $2), $4, $5, $6, $7) \
     RETURNING id, conversation_id, client_message_id, seq, role, content, trust, trace_id, created_at";

fn trust_str(t: TrustClass) -> &'static str {
    match t {
        TrustClass::Owner => "owner",
        TrustClass::Tool => "tool",
        TrustClass::Untrusted => "untrusted",
    }
}

fn parse_trust(s: &str) -> Result<TrustClass> {
    match s {
        "owner" => Ok(TrustClass::Owner),
        "tool" => Ok(TrustClass::Tool),
        "untrusted" => Ok(TrustClass::Untrusted),
        other => Err(PairError::new(
            ErrorCode::Internal,
            format!("corrupt trust value {other}"),
        )),
    }
}

fn row_to_message(row: &PgRow) -> Result<StoredMessage> {
    let get = |e: sqlx::Error| db_err(&e);
    let trust: String = row.try_get("trust").map_err(get)?;
    Ok(StoredMessage {
        id: row.try_get("id").map_err(get)?,
        conversation: ConversationId(row.try_get("conversation_id").map_err(get)?),
        client_message_id: row.try_get("client_message_id").map_err(get)?,
        seq: row.try_get("seq").map_err(get)?,
        role: row.try_get("role").map_err(get)?,
        content: row.try_get("content").map_err(get)?,
        trust: parse_trust(&trust)?,
        trace: TraceId(row.try_get("trace_id").map_err(get)?),
        created_at: row.try_get("created_at").map_err(get)?,
    })
}
