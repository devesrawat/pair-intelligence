//! The stored answer of a finished turn, returned to a replay instead of a second paid call.

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::ConversationId;
use pair_models::provider::store::StoredMessage;

use super::request::TurnResponse;
use super::{COST_RECONCILED, COST_UNRESOLVED};
use crate::services::Services;

pub const ASSISTANT_SUFFIX: &str = "assistant";

pub fn assistant_id(user_client_id: &str) -> String {
    format!("{user_client_id}:{ASSISTANT_SUFFIX}")
}

type CallRow = (String, String, String);

/// Rebuild the response of the turn whose assistant message is `answer`.
pub async fn stored_answer(
    svc: &Services,
    conversation: ConversationId,
    answer: &StoredMessage,
) -> Result<TurnResponse> {
    let call: Option<CallRow> = sqlx::query_as(
        "SELECT COALESCE(resolved_model, requested_model), route_reason, cost_state \
         FROM model_calls WHERE response_message_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(answer.id)
    .fetch_optional(svc.store.pool())
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "could not read the stored model call");
        PairError::new(ErrorCode::Internal, "stored answer could not be read")
    })?;
    let (resolved_model, route_reason, cost_state) = call.ok_or_else(|| {
        PairError::new(
            ErrorCode::Conflict,
            "this turn has a stored answer but no recorded model call; read the conversation",
        )
    })?;
    Ok(TurnResponse {
        text: answer.content.clone(),
        resolved_model,
        route_reason,
        cost_state: if cost_state == "reconciled" {
            COST_RECONCILED
        } else {
            COST_UNRESOLVED
        },
        trace_id: answer.trace.0.to_string(),
        conversation_id: conversation.to_string(),
        message_id: answer.id.to_string(),
        replayed: true,
    })
}
