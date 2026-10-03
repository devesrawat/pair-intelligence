//! Opening a turn: resolve the conversation and its data class, claim the logical turn, store the
//! user message and read the history the model will see. Nothing here talks to a classifier or a
//! provider.

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TaskId, TraceId};
use pair_core::types::{DataClass, TrustClass};
use pair_models::provider::store::{NewMessage, StoredMessage};

use super::claim::{content_hash, Claim, ClaimStore, CLAIM_LEASE_MARGIN};
use super::replay::{assistant_id, stored_answer};
use super::request::{class_rank, TurnResponse, ValidTurn};
use super::{RECENT_MESSAGES, TITLE_CHARS};
use crate::services::Services;

/// A turn that is ours to run.
pub struct Fresh {
    pub conversation: ConversationId,
    /// The most recent messages before this turn's own, oldest first (a bounded read).
    pub history: Vec<StoredMessage>,
    pub user_client_id: String,
    /// The conversation's class, read AFTER the history: the class only rises, so it covers every
    /// message the history read can have seen.
    pub class: DataClass,
    /// Set when the turn holds a claim that must be closed when it ends.
    pub claim: Option<TaskId>,
}

pub enum Opened {
    Fresh(Fresh),
    /// A replay of a finished turn: the stored answer, no new work.
    Answered(TurnResponse),
}

/// Resolve the conversation and its class before anything else is written: a turn declared below
/// the class the conversation already holds is refused (it would relabel earlier history), a
/// higher one raises it.
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

/// Claim the logical turn. Only a turn with a caller-chosen identity (conversation and
/// `client_message_id`) has one; every other turn is a fresh task.
async fn claim_turn(
    svc: &Services,
    turn: &ValidTurn,
    task: TaskId,
    conversation: ConversationId,
) -> Result<Option<Claim>> {
    let (Some(_), Some(client_id)) = (turn.conversation, turn.client_message_id.as_deref()) else {
        return Ok(None);
    };
    let hash = content_hash(&turn.message, &format!("{:?}", turn.data_class));
    let lease = svc.turn_budget + CLAIM_LEASE_MARGIN;
    let claim = ClaimStore::new(svc.store.pool().clone())
        .claim(task, conversation, client_id, &hash, lease)
        .await?;
    Ok(Some(claim))
}

fn refusal(claim: Claim) -> Option<PairError> {
    match claim {
        Claim::InProgress => Some(PairError::new(
            ErrorCode::Conflict,
            "this turn is already being processed; retry to read its answer",
        )),
        Claim::ContentMismatch => Some(PairError::new(
            ErrorCode::Conflict,
            "this client_message_id was already used with different content",
        )),
        Claim::Acquired | Claim::Done => None,
    }
}

pub async fn open_conversation(
    svc: &Services,
    turn: &ValidTurn,
    task: TaskId,
    trace: TraceId,
) -> Result<Opened> {
    let conversation = admit_conversation(svc, turn, trace).await?;
    let claim = claim_turn(svc, turn, task, conversation).await?;
    if let Some(error) = claim.and_then(refusal) {
        return Err(error);
    }
    let claimed = (claim == Some(Claim::Acquired)).then_some(task);
    let finished = claim == Some(Claim::Done);
    let opened = read_turn(svc, turn, task, trace, conversation, (claimed, finished)).await;
    match (&opened, claimed) {
        (Ok(Opened::Fresh(_)), _) | (_, None) => {}
        (Ok(Opened::Answered(_)), Some(task)) => close_claim(svc, task, true).await,
        (Err(_), Some(task)) => close_claim(svc, task, false).await,
    }
    opened
}

async fn close_claim(svc: &Services, task: TaskId, done: bool) {
    ClaimStore::new(svc.store.pool().clone())
        .finish(task, done)
        .await;
}

/// Store the user message and read the history. A message that already exists means a replay:
/// different text is refused, and a stored answer is returned as it is.
async fn read_turn(
    svc: &Services,
    turn: &ValidTurn,
    task: TaskId,
    trace: TraceId,
    conversation: ConversationId,
    (claim, finished): (Option<TaskId>, bool),
) -> Result<Opened> {
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
    if !inserted {
        if stored.content != turn.message {
            return Err(PairError::new(
                ErrorCode::Conflict,
                "this client_message_id was already used with different content",
            ));
        }
        let answer = svc
            .store
            .find_message(conversation, &assistant_id(&user_client_id))
            .await?;
        if let Some(answer) = answer {
            return Ok(Opened::Answered(
                stored_answer(svc, conversation, &answer).await?,
            ));
        }
    }
    if finished {
        return Err(PairError::new(
            ErrorCode::Conflict,
            "this turn is recorded as finished but its answer is missing; read the conversation",
        ));
    }
    // One extra row: this turn's own message is among the newest and is dropped below.
    let limit = u32::try_from(RECENT_MESSAGES + 1).unwrap_or(u32::MAX);
    let mut history = svc.store.recent_messages(conversation, limit).await?;
    history.retain(|m| m.id != stored.id);
    let excess = history.len().saturating_sub(RECENT_MESSAGES);
    history.drain(..excess);
    let class = svc.store.conversation_data_class(conversation).await?;
    Ok(Opened::Fresh(Fresh {
        conversation,
        history,
        user_client_id,
        class,
        claim,
    }))
}
