//! Bounded message reads for the turn path: the last few messages and one message by identity,
//! instead of loading a whole conversation on every turn.
use super::{
    db_err, row_to_message, ConversationStore, StoredMessage, SELECT_BY_CLIENT_ID, SELECT_COLUMNS,
};
use pair_core::error::Result;
use pair_core::ids::ConversationId;

impl ConversationStore {
    /// The last `limit` messages of a conversation, oldest first.
    pub async fn recent_messages(
        &self,
        conversation: ConversationId,
        limit: u32,
    ) -> Result<Vec<StoredMessage>> {
        let rows = sqlx::query(&format!(
            "SELECT * FROM ({SELECT_COLUMNS} WHERE conversation_id = $1 ORDER BY seq DESC LIMIT $2) \
             recent ORDER BY seq"
        ))
        .bind(conversation.0)
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|e| db_err(&e))?;
        rows.iter().map(row_to_message).collect()
    }

    /// One message by its caller-supplied identity (for example `{id}:assistant`).
    pub async fn find_message(
        &self,
        conversation: ConversationId,
        client_message_id: &str,
    ) -> Result<Option<StoredMessage>> {
        let row = sqlx::query(SELECT_BY_CLIENT_ID)
            .bind(conversation.0)
            .bind(client_message_id)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| db_err(&e))?;
        row.as_ref().map(row_to_message).transpose()
    }
}
