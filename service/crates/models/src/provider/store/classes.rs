//! A conversation's data class: the highest class any of its turns declared. It only rises.
use super::{db_err, ConversationStore};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TraceId};
use pair_core::types::DataClass;

fn class_str(class: DataClass) -> &'static str {
    match class {
        DataClass::Public => "public",
        DataClass::Personal => "personal",
        DataClass::Sensitive => "sensitive",
        DataClass::Employer => "employer",
    }
}

fn parse_class(s: &str) -> Result<DataClass> {
    match s {
        "public" => Ok(DataClass::Public),
        "personal" => Ok(DataClass::Personal),
        "sensitive" => Ok(DataClass::Sensitive),
        "employer" => Ok(DataClass::Employer),
        other => Err(PairError::new(
            ErrorCode::Internal,
            format!("corrupt data_class {other}"),
        )),
    }
}

fn not_found(conversation: ConversationId) -> PairError {
    PairError::new(
        ErrorCode::NotFound,
        format!("conversation {conversation} not found"),
    )
}

impl ConversationStore {
    /// Create a conversation that records the class its first turn declared.
    pub async fn create_conversation_with_class(
        &self,
        title: &str,
        trace: TraceId,
        class: DataClass,
    ) -> Result<ConversationId> {
        let id = ConversationId::new();
        sqlx::query(
            "INSERT INTO conversations (id, title, trace_id, data_class) VALUES ($1, $2, $3, $4)",
        )
        .bind(id.0)
        .bind(title)
        .bind(trace.0)
        .bind(class_str(class))
        .execute(self.pool())
        .await
        .map_err(|e| db_err(&e))?;
        Ok(id)
    }

    /// The stored class, or `NotFound`.
    pub async fn conversation_data_class(&self, conversation: ConversationId) -> Result<DataClass> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT data_class FROM conversations WHERE id = $1")
                .bind(conversation.0)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| db_err(&e))?;
        parse_class(&stored.ok_or_else(|| not_found(conversation))?)
    }

    /// Raise the stored class to `class` when `class` is higher; never lowers it. One statement,
    /// so concurrent turns cannot lose a raise. Returns the class now stored.
    pub async fn raise_data_class(
        &self,
        conversation: ConversationId,
        class: DataClass,
    ) -> Result<DataClass> {
        let stored: Option<String> = sqlx::query_scalar(
            "UPDATE conversations SET data_class = CASE \
               WHEN pair_data_class_rank($2) > pair_data_class_rank(data_class) THEN $2 \
               ELSE data_class END \
             WHERE id = $1 RETURNING data_class",
        )
        .bind(conversation.0)
        .bind(class_str(class))
        .fetch_optional(self.pool())
        .await
        .map_err(|e| db_err(&e))?;
        parse_class(&stored.ok_or_else(|| not_found(conversation))?)
    }
}
