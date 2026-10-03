//! `POST /v1/turn`: refuse disallowed data classes, classify (shadow), plan a route within the
//! task cap, compile context, call the provider through the budget, persist with a cost state.

mod persist;
pub mod recording;
pub mod request;

use std::time::Instant;

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TraceId};
use pair_core::money::Micros;
use pair_core::traits::{BudgetEx, ContextCompiler};
use pair_core::types::{
    ClassificationInput, ModelLimits, ModelMessage, ModelRequest, TaskContext, TaskProfile,
    TrustClass,
};
use pair_models::classification::pipeline::{PipelineOutcome, PipelineRequest};
use pair_models::provider::store::{NewMessage, StoredMessage};
use pair_models::router::RouteConstraints;
use pair_workflows::calls::{ModelCaller, ModelPlanner};
use pair_workflows::limits::{RunLimits, MAX_TOOL_CALLS};
use serde_json::json;

use crate::services::Services;
use persist::{persist_attempts, CallContext};
use recording::{AttemptOutcome, AttemptTrace, TracedBudget, TracedProvider, TurnTrace};
pub use request::{TurnRequest, TurnResponse, ValidTurn};

const RECENT_MESSAGES: usize = 12;
const SUMMARY_MESSAGES: usize = 3;
const SUMMARY_CHARS: usize = 160;
const TITLE_CHARS: usize = 60;
const ROUTING_BYTES_PER_TOKEN: usize = 4;
const CONTEXT_OVERHEAD_TOKENS: u64 = 400;
const WORKFLOWS: [&str; 5] = [
    "coding",
    "research",
    "planning",
    "memory_recall",
    "transformation",
];
const OUTPUT_CONTRACT: &str = "Answer the owner's latest request directly and concisely.";
const POLICY_SUMMARY: &str =
    "No tools are available in this turn. Never claim to have run commands or changed anything.";
const TURN_INTENT: &str = "turn";
const COST_RECONCILED: &str = "reconciled";
const COST_UNRESOLVED: &str = "unresolved";
const AUDIT_ACTOR_FALLBACK: &str = "unknown";

/// A plan fixed by the route step; `ModelCaller` walks it in order.
struct FixedPlan(Vec<String>);

impl ModelPlanner for FixedPlan {
    fn attempt_order(&self, _profile: &TaskProfile, _remaining: Micros) -> Result<Vec<String>> {
        Ok(self.0.clone())
    }
}

pub async fn run_turn(
    svc: &Services,
    actor: &str,
    trace: TraceId,
    turn: ValidTurn,
) -> Result<TurnResponse> {
    let task = turn.task_id();
    let (conversation, history, user_client_id) =
        open_conversation(svc, &turn, task, trace).await?;
    let recent = recent_messages(&history);

    let outcome = route(svc, &turn, task, &recent).await?;
    record_shadow(svc, actor, trace, task, &outcome).await;
    let plan = vet_plan(svc, outcome.plan.attempt_order())?;
    let route_reason = format!(
        "{}; classifier: {}",
        outcome.plan.decision.reason, outcome.classifier_note
    );

    let limits_model = model_limits(svc, &plan)?;
    let compiled = compile(svc, &turn, &recent, &limits_model)?;
    let request = ModelRequest {
        model_id: String::new(),
        messages: compiled.messages,
        max_output_tokens: u32::try_from(limits_model.max_output_tokens).unwrap_or(u32::MAX),
        deadline_ms: u64::try_from(svc.turn_budget.as_millis()).unwrap_or(u64::MAX),
        data_class: turn.data_class,
        task,
        trace,
    };

    let attempt_trace = TurnTrace::default();
    let budget = TracedBudget {
        inner: &svc.budget,
        attempts: &svc.attempts,
        trace: attempt_trace.clone(),
    };
    let provider = TracedProvider {
        inner: svc.provider.as_ref(),
        trace: attempt_trace.clone(),
    };
    let run_limits = RunLimits::with_deadline(MAX_TOOL_CALLS, Instant::now() + svc.turn_budget);
    let planner = FixedPlan(plan);
    let caller = ModelCaller {
        provider: &provider,
        budget: &budget,
        prices: svc.registry.as_ref(),
        planner: &planner,
        limits: &run_limits,
        kind: turn.kind,
        intent: TURN_INTENT,
    };
    let generated = caller.generate(request.clone()).await;

    let call_ctx = CallContext {
        store: &svc.store,
        registry: &svc.registry,
        conversation,
        trace,
        route_reason: &route_reason,
        assistant_client_id: format!("{user_client_id}:assistant"),
    };
    let attempts = attempt_trace.snapshot();
    let persisted = persist_attempts(&call_ctx, &request, &attempts).await?;
    let response = generated.map_err(|e| {
        tracing::warn!(%task, %trace, code = ?e.code, "turn ended without a response");
        root_cause(e, &attempts)
    })?;
    let persisted = persisted.ok_or_else(|| {
        PairError::new(
            ErrorCode::Internal,
            "provider answered but nothing was recorded",
        )
    })?;
    Ok(TurnResponse {
        text: response.text,
        resolved_model: response.resolved_model,
        route_reason,
        cost_state: if persisted.reconciled {
            COST_RECONCILED
        } else {
            COST_UNRESOLVED
        },
        trace_id: trace.0.to_string(),
        conversation_id: conversation.to_string(),
        message_id: persisted.message_id.to_string(),
    })
}

/// A budget refusal that follows a failed provider call is a consequence, not the cause: the failed
/// attempt's unresolved reservation is what used up the task cap. Report the provider failure.
fn root_cause(error: PairError, attempts: &[AttemptTrace]) -> PairError {
    if error.code != ErrorCode::BudgetExceeded {
        return error;
    }
    let failed = attempts.iter().find_map(|a| match &a.outcome {
        Some(AttemptOutcome::Failed { code, .. })
            if matches!(
                code,
                ErrorCode::ProviderUnavailable | ErrorCode::ProviderTimeout
            ) =>
        {
            Some(*code)
        }
        _ => None,
    });
    match failed {
        Some(code) => PairError::new(
            code,
            "the provider call failed and the task budget stopped any further attempt",
        ),
        None => error,
    }
}

async fn open_conversation(
    svc: &Services,
    turn: &ValidTurn,
    task: pair_core::ids::TaskId,
    trace: TraceId,
) -> Result<(ConversationId, Vec<StoredMessage>, String)> {
    let (conversation, history) = match turn.conversation {
        Some(c) => (c, svc.store.list_messages(c).await?),
        None => {
            let title: String = turn.message.chars().take(TITLE_CHARS).collect();
            (
                svc.store.create_conversation(&title, trace).await?,
                Vec::new(),
            )
        }
    };
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
    let assistant_id = format!("{user_client_id}:assistant");
    if !inserted && history.iter().any(|m| m.client_message_id == assistant_id) {
        return Err(PairError::new(
            ErrorCode::Conflict,
            "this turn was already answered; read the conversation instead of repeating it",
        ));
    }
    let history = history.into_iter().filter(|m| m.id != stored.id).collect();
    Ok((conversation, history, user_client_id))
}

fn recent_messages(history: &[StoredMessage]) -> Vec<ModelMessage> {
    let skip = history.len().saturating_sub(RECENT_MESSAGES);
    history
        .iter()
        .skip(skip)
        .map(|m| ModelMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            trust: m.trust,
        })
        .collect()
}

fn summarize(recent: &[ModelMessage]) -> String {
    let skip = recent.len().saturating_sub(SUMMARY_MESSAGES);
    recent
        .iter()
        .skip(skip)
        .map(|m| {
            let text: String = m.content.chars().take(SUMMARY_CHARS).collect();
            format!("{}: {text}", m.role)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn route(
    svc: &Services,
    turn: &ValidTurn,
    task: pair_core::ids::TaskId,
    recent: &[ModelMessage],
) -> Result<PipelineOutcome> {
    let history_bytes: usize = recent.iter().map(|m| m.content.len()).sum();
    let est_tokens = u64::try_from((turn.message.len() + history_bytes) / ROUTING_BYTES_PER_TOKEN)
        .unwrap_or(u64::MAX)
        .saturating_add(CONTEXT_OVERHEAD_TOKENS);
    // The router is given the task kind's own cap, never its default $0.10.
    let constraints = RouteConstraints {
        remaining_budget: BudgetEx::task_cap(svc.budget.as_ref(), turn.kind),
        ..svc.pipeline.router().default_constraints()
    };
    svc.pipeline
        .route(
            svc.budget.as_ref(),
            PipelineRequest {
                input: ClassificationInput {
                    request: turn.message.clone(),
                    recent_summary: summarize(recent),
                    project_type: turn.project_type.clone(),
                    workflows: WORKFLOWS.iter().map(|w| (*w).to_owned()).collect(),
                    task: request::classifier_task(task),
                },
                data_class: turn.data_class,
                needs_tools: false,
                est_input_tokens: est_tokens,
                constraints: Some(constraints),
            },
        )
        .await
}

/// Shadow mode: the recommendation is recorded and never acted on. A failed write is logged,
/// not surfaced: classification must not be able to fail a turn.
async fn record_shadow(
    svc: &Services,
    actor: &str,
    trace: TraceId,
    task: pair_core::ids::TaskId,
    outcome: &PipelineOutcome,
) {
    let detail = json!({
        "task": task.to_string(),
        "classifier_note": outcome.classifier_note,
        "classifier_cost_micros": outcome.classifier_cost.map(|c| c.0),
        "classification": outcome.classification.as_ref().map(|c| json!({
            "model_version": c.model_version,
            "question_version": c.question_version,
            "intent": c.intent,
            "difficulty": c.difficulty,
            "intent_confidence": c.intent_confidence,
            "difficulty_confidence": c.difficulty_confidence,
            "request_id": c.request_id,
            "latency_ms": c.latency_ms,
        })),
        "recommendation": outcome.plan.recommendation.as_ref().map(|r| json!({
            "tier": r.tier, "applied": r.applied, "note": r.note,
        })),
        "chosen_model": outcome.plan.decision.model_id,
    });
    let actor = if actor.is_empty() {
        AUDIT_ACTOR_FALLBACK
    } else {
        actor
    };
    if let Err(e) = svc
        .store
        .record_audit_event(trace, actor, "routing_decision", &task.to_string(), &detail)
        .await
    {
        tracing::warn!(error = %e, "could not record the routing decision");
    }
}

/// An unverified model id stops the call (spec 6: a refusal never escalates to a pricier model).
/// Doing it here, before the first reservation, also avoids leaving a worst-case reservation
/// unresolved for a call the adapter was always going to refuse. A plan is cut at the first
/// unverified entry: nothing past it is tried.
fn vet_plan(svc: &Services, order: Vec<String>) -> Result<Vec<String>> {
    let mut vetted = Vec::with_capacity(order.len());
    for id in order {
        let verified = svc.registry.get(&id).is_some_and(|e| e.id_verified);
        if !verified && !svc.allow_unverified_ids {
            if vetted.is_empty() {
                return Err(PairError::new(
                    ErrorCode::ProviderDisallowed,
                    format!("model id {id} is unverified against the provider catalog; refusing"),
                ));
            }
            break;
        }
        vetted.push(id);
    }
    Ok(vetted)
}

fn model_limits(svc: &Services, plan: &[String]) -> Result<ModelLimits> {
    let configured = svc.pipeline.router().config().max_output_tokens;
    let mut limits = ModelLimits {
        context_tokens: u64::MAX,
        max_output_tokens: configured,
    };
    for id in plan {
        let entry = svc.registry.get(id).ok_or_else(|| {
            PairError::new(
                ErrorCode::InvalidInput,
                format!("routed model {id} is not in the provider registry"),
            )
        })?;
        limits.context_tokens = limits.context_tokens.min(entry.context_tokens);
        limits.max_output_tokens = limits.max_output_tokens.min(entry.max_output_tokens);
    }
    Ok(limits)
}

fn compile(
    svc: &Services,
    turn: &ValidTurn,
    recent: &[ModelMessage],
    limits: &ModelLimits,
) -> Result<pair_core::types::CompiledContext> {
    let context = TaskContext {
        objective: turn.message.clone(),
        output_contract: OUTPUT_CONTRACT.into(),
        policy_summary: POLICY_SUMMARY.into(),
        recent: recent.to_vec(),
        tool_results: Vec::new(),
    };
    svc.compiler.compile(&context, limits, &[])
}
