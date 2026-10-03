//! `POST /v1/turn`: refuse disallowed data classes, classify (shadow), plan a route within the
//! task cap, compile context, call the provider through the budget, persist with a cost state.

mod claim;
mod guard;
mod open;
mod persist;
pub mod recording;
mod replay;
pub mod request;

use std::sync::Arc;
use std::time::Instant;

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TraceId};
use pair_core::money::Micros;
use pair_core::traits::{BudgetEx, ContextCompiler};
use pair_core::types::{
    ClassificationInput, ModelLimits, ModelMessage, ModelRequest, ModelResponse, TaskContext,
    TaskProfile,
};
use pair_models::classification::pipeline::{PipelineOutcome, PipelineRequest};
use pair_models::provider::store::StoredMessage;
use pair_models::router::RouteConstraints;
use pair_workflows::calls::{ModelCaller, ModelPlanner};
use pair_workflows::limits::{RunLimits, MAX_TOOL_CALLS};
use serde_json::json;

use crate::services::Services;
use claim::ClaimStore;
use guard::{settle_dangling, TurnGuard};
use open::{open_conversation, Fresh, Opened};
use persist::{persist_attempts, CallContext};
use recording::{AttemptOutcome, AttemptTrace, TracedBudget, TracedProvider, TurnTrace};
pub use request::{TurnRequest, TurnResponse, ValidTurn};

const RECENT_MESSAGES: usize = 12;
const TITLE_CHARS: usize = 60;
const SUMMARY_MESSAGES: usize = 3;
const SUMMARY_CHARS: usize = 160;
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

/// Everything fixed before the first provider call: the request, the vetted model order and why.
struct Prepared {
    request: ModelRequest,
    plan: Vec<String>,
    route_reason: String,
}

/// Identity of one turn, shared by the stages that follow the provider call.
struct TurnIds {
    task: pair_core::ids::TaskId,
    trace: TraceId,
    conversation: ConversationId,
    user_client_id: String,
}

pub async fn run_turn(
    svc: &Services,
    actor: &str,
    trace: TraceId,
    turn: ValidTurn,
) -> Result<TurnResponse> {
    // The turn budget covers the whole turn (classifier and database time included), not just the
    // provider attempts.
    let started = Instant::now();
    let task = turn.task_id();
    let fresh = match open_conversation(svc, &turn, task, trace).await? {
        Opened::Answered(response) => return Ok(response),
        Opened::Fresh(fresh) => fresh,
    };
    let claim = fresh.claim;
    let claims = ClaimStore::new(svc.store.pool().clone());
    let attempt_trace = TurnTrace::default();
    // If this future is dropped from here on, the guard settles what was reserved as unresolved
    // and releases the claim.
    let mut guard = TurnGuard::arm(
        Arc::clone(&svc.budget),
        attempt_trace.clone(),
        svc.turns.clone(),
        claim.map(|task| (claims.clone(), task)),
    );
    let result = run_claimed(svc, actor, trace, turn, fresh, (&attempt_trace, started)).await;
    if let Some(task) = claim {
        claims.finish(task, result.is_ok()).await;
    }
    guard.disarm();
    result
}

/// The paid part of a turn: route, call the provider through the budget, persist.
async fn run_claimed(
    svc: &Services,
    actor: &str,
    trace: TraceId,
    turn: ValidTurn,
    fresh: Fresh,
    (attempt_trace, started): (&TurnTrace, Instant),
) -> Result<TurnResponse> {
    let task = turn.task_id();
    // Everything below (routing, the classifier summary, the provider call) runs under the
    // conversation's highest class, not just this turn's.
    let turn = ValidTurn {
        data_class: fresh.class,
        ..turn
    };
    let recent = recent_messages(&fresh.history);

    let outcome = route(svc, &turn, task, &recent).await?;
    record_shadow(svc, actor, trace, task, &outcome).await;
    let prepared = prepare(svc, &turn, (task, trace), &recent, &outcome, started)?;

    let generated = generate(svc, &turn, &prepared, attempt_trace, started).await;
    let ids = TurnIds {
        task,
        trace,
        conversation: fresh.conversation,
        user_client_id: fresh.user_client_id,
    };
    finish(svc, &ids, &prepared, attempt_trace, generated).await
}

fn prepare(
    svc: &Services,
    turn: &ValidTurn,
    (task, trace): (pair_core::ids::TaskId, TraceId),
    recent: &[ModelMessage],
    outcome: &PipelineOutcome,
    started: Instant,
) -> Result<Prepared> {
    let plan = vet_plan(svc, outcome.plan.attempt_order())?;
    let route_reason = format!(
        "{}; classifier: {}",
        outcome.plan.decision.reason, outcome.classifier_note
    );
    let limits_model = model_limits(svc, &plan)?;
    let compiled = compile(svc, turn, recent, &limits_model)?;
    let request = ModelRequest {
        model_id: String::new(),
        messages: compiled.messages,
        max_output_tokens: u32::try_from(limits_model.max_output_tokens).unwrap_or(u32::MAX),
        deadline_ms: u64::try_from(
            svc.turn_budget
                .saturating_sub(started.elapsed())
                .as_millis(),
        )
        .unwrap_or(u64::MAX),
        data_class: turn.data_class,
        task,
        trace,
    };
    Ok(Prepared {
        request,
        plan,
        route_reason,
    })
}

async fn generate(
    svc: &Services,
    turn: &ValidTurn,
    prepared: &Prepared,
    attempt_trace: &TurnTrace,
    started: Instant,
) -> Result<ModelResponse> {
    let budget = TracedBudget {
        inner: &svc.budget,
        attempts: &svc.attempts,
        trace: attempt_trace.clone(),
    };
    let provider = TracedProvider {
        inner: svc.provider.as_ref(),
        trace: attempt_trace.clone(),
    };
    let run_limits = RunLimits::with_deadline(MAX_TOOL_CALLS, started + svc.turn_budget);
    let planner = FixedPlan(prepared.plan.clone());
    let caller = ModelCaller {
        provider: &provider,
        budget: &budget,
        prices: svc.registry.as_ref(),
        planner: &planner,
        limits: &run_limits,
        kind: turn.kind,
        intent: TURN_INTENT,
    };
    let generated = caller.generate(prepared.request.clone()).await;
    // A reconcile that failed (or never ran) must not leave the reservation `held`.
    settle_dangling(&svc.budget, attempt_trace).await;
    generated
}

/// Persist every attempt, then turn the provider result into the response.
async fn finish(
    svc: &Services,
    ids: &TurnIds,
    prepared: &Prepared,
    attempt_trace: &TurnTrace,
    generated: Result<ModelResponse>,
) -> Result<TurnResponse> {
    let call_ctx = CallContext {
        store: &svc.store,
        registry: &svc.registry,
        conversation: ids.conversation,
        trace: ids.trace,
        route_reason: &prepared.route_reason,
        assistant_client_id: format!("{}:assistant", ids.user_client_id),
    };
    let attempts = attempt_trace.snapshot();
    let persisted = persist_attempts(&call_ctx, &prepared.request, &attempts).await?;
    let response = generated.map_err(|e| {
        tracing::warn!(task = %ids.task, trace = %ids.trace, code = ?e.code, "turn ended without a response");
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
        route_reason: prepared.route_reason.clone(),
        cost_state: if persisted.reconciled {
            COST_RECONCILED
        } else {
            COST_UNRESOLVED
        },
        trace_id: ids.trace.0.to_string(),
        conversation_id: ids.conversation.to_string(),
        message_id: persisted.message_id.to_string(),
        replayed: false,
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
