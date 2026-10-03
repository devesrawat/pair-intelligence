//! Opening a turn: resolve the conversation and its data class, store the user message and read the
//! history the model will see. Nothing here talks to a classifier or a provider.

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TaskId, TraceId};
use pair_core::types::{DataClass, TrustClass};
use pair_models::provider::store::{NewMessage, StoredMessage};

use super::request::{class_rank, ValidTurn};
use crate::services::Services;

const TITLE_CHARS: usize = 60;

/// What `open_conversation` hands the rest of the turn.
pub struct Opened {
    pub conversation: ConversationId,
    pub history: Vec<StoredMessage>,
    pub user_client_id: String,
    /// The conversation's class, read AFTER the history: the class only rises, so it covers every
    /// message the history read can have seen.
    pub class: DataClass,
}

/// Resolve the conversation and its class before anything is written: a turn declared below the
/// class the conversation already holds is refused (it would relabel earlier history), a higher
/// one raises it.
async fn admit_conversation(
    svc: &Services,
    turn: &ValidTurn,
    trace: TraceId,
) -> Result<ConversationId> {
    let Some(conversation) = turn.conversation else {
        let title: String = turn.message.chars().take(TITLE_CHARS).collect();
        return svc
            .store
            .create_conversation_with_class(&title, trace, turn.data_class)
            .await;
    };
    let stored = svc.store.conversation_data_class(conversation).await?;
    if class_rank(turn.data_class) < class_rank(stored) {
        return Err(PairError::new(
            ErrorCode::PolicyDenied,
            format!(
                "this conversation already holds {stored:?} data; a turn cannot be declared {:?}",
                turn.data_class
            ),
        ));
    }
    svc.store
        .raise_data_class(conversation, turn.data_class)
        .await?;
    Ok(conversation)
}

pub async fn open_conversation(
    svc: &Services,
    turn: &ValidTurn,
    task: TaskId,
    trace: TraceId,
) -> Result<Opened> {
    let conversation = admit_conversation(svc, turn, trace).await?;
    let user_client_id = turn
        .client_message_id
        .clone()
        .unwrap_or_else(|| format!("{task}:user"));
    let (stored, inserted) = svc
        .store
        .append_message(NewMessage {
            conversation,
            client_message_id: user_client_id.clone(),
            role: "user".into(),
            content: turn.message.clone(),
            trust: TrustClass::Owner,
            trace,
        })
        .await?;
    let history = svc.store.list_messages(conversation).await?;
    let class = svc.store.conversation_data_class(conversation).await?;
    let assistant_id = format!("{user_client_id}:assistant");
    if !inserted && history.iter().any(|m| m.client_message_id == assistant_id) {
        return Err(PairError::new(
            ErrorCode::Conflict,
            "this turn was already answered; read the conversation instead of repeating it",
        ));
    }
    let history = history.into_iter().filter(|m| m.id != stored.id).collect();
    Ok(Opened {
        conversation,
        history,
        user_client_id,
        class,
    })
}
