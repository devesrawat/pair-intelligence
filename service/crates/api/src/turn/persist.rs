//! Persistence of a turn: one `model_calls` row per attempt (with its cost state) and the
//! assistant message.

use pair_core::error::Result;
use pair_core::ids::{ConversationId, TraceId};
use pair_core::types::{ModelRequest, TrustClass};
use pair_models::provider::store::{ConversationStore, CostState, ModelCallRecord, NewMessage};
use pair_models::provider::ProviderRegistry;
use uuid::Uuid;

use super::recording::{AttemptOutcome, AttemptTrace};

pub struct Persisted {
    pub message_id: Uuid,
    pub reconciled: bool,
}

pub struct CallContext<'a> {
    pub store: &'a ConversationStore,
    pub registry: &'a ProviderRegistry,
    pub conversation: ConversationId,
    pub trace: TraceId,
    pub route_reason: &'a str,
    pub assistant_client_id: String,
}

fn provider_name(registry: &ProviderRegistry, model: &str) -> String {
    registry.get(model).map_or_else(
        || "unknown".to_owned(),
        |e| format!("{:?}", e.provider).to_lowercase(),
    )
}

/// Record every attempt, failures included, so no provider call is missing from the audit trail.
/// Returns the stored assistant message when an attempt succeeded.
pub async fn persist_attempts(
    ctx: &CallContext<'_>,
    template: &ModelRequest,
    attempts: &[AttemptTrace],
) -> Result<Option<Persisted>> {
    let mut persisted = None;
    for attempt in attempts {
        let Some(model) = attempt.model_id.as_deref() else {
            continue;
        };
        let mut req = template.clone();
        req.model_id = model.to_owned();
        let provider = provider_name(ctx.registry, model);
        match &attempt.outcome {
            Some(AttemptOutcome::Succeeded(resp)) => {
                let (message, _) = ctx
                    .store
                    .append_message(NewMessage {
                        conversation: ctx.conversation,
                        client_message_id: ctx.assistant_client_id.clone(),
                        role: "assistant".into(),
                        content: resp.text.clone(),
                        trust: TrustClass::Tool,
                        trace: ctx.trace,
                    })
                    .await?;
                let mut record = ModelCallRecord::from_response(
                    &req,
                    &provider,
                    ctx.route_reason,
                    Some(ctx.conversation),
                    resp,
                );
                let reconciled = attempt.settled == Some(true);
                record.reservation = Some(attempt.reservation);
                record.response_message_id = Some(message.id);
                if reconciled {
                    record.cost_state = CostState::Reconciled;
                }
                ctx.store.record_model_call(&record).await?;
                persisted = Some(Persisted {
                    message_id: message.id,
                    reconciled,
                });
            }
            Some(AttemptOutcome::Failed { code, latency_ms }) => {
                let mut record = ModelCallRecord::from_error(
                    &req,
                    &provider,
                    ctx.route_reason,
                    Some(ctx.conversation),
                    *code,
                    *latency_ms,
                );
                record.reservation = Some(attempt.reservation);
                ctx.store.record_model_call(&record).await?;
            }
            None => {}
        }
    }
    Ok(persisted)
}
