//! `POST /v1/budget/reserve` and `POST /v1/budget/reconcile` (OpenClaw adapter).
//!
//! The adapter reserves in `before_model_resolve` and settles from `llm_output.usage`, but its
//! numbers are never authority: the server sizes the hold from the registry price (the client's
//! figure can only raise it), counts a settlement at `max(reported, tokens x registry price)`,
//! bounds it, and binds each reservation to the task it was made for. Kind, category and price
//! version are always explicit: nothing is inferred.

use axum::extract::State;
use axum::Json;
use pair_core::error::{ErrorCode, PairError};
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::Micros;
use pair_core::types::{BudgetCategory, ReserveRequest, TaskKind, UsageReport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::ok;
use crate::adapter_budget;
use crate::error::ApiError;
use crate::json::ApiJson;
use crate::state::AppState;

const STATE_SETTLED: &str = "settled";
const STATE_UNRESOLVED: &str = "unresolved";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveBody {
    /// Identity of the task or run the money is held for; a reconcile must present the same one.
    pub task_id: Uuid,
    /// Model the call will use. Its registry price sizes the hold. Omitted: the hold is sized for
    /// the dearest model priced under `price_version`.
    #[serde(default)]
    pub model_id: Option<String>,
    pub kind: TaskKind,
    pub category: BudgetCategory,
    /// May raise the hold above the server's worst case, never lower it.
    pub max_cost_micros: i64,
    pub price_version: String,
}

#[derive(Debug, Serialize)]
struct ReserveResponse {
    reservation_id: String,
    task_id: String,
    model_id: Option<String>,
    kind: TaskKind,
    category: BudgetCategory,
    price_version: String,
    /// What is actually held: `max(client figure, server worst case)`.
    max_cost_micros: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileBody {
    pub reservation_id: Uuid,
    /// Must equal the `task_id` the reservation was made for.
    pub task_id: Uuid,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `null` (or absent): the cost is unknown and the reservation stays unresolved.
    #[serde(default)]
    pub actual_cost_micros: Option<i64>,
    pub price_version: String,
}

#[derive(Debug, Serialize)]
struct ReconcileResponse {
    entry_id: String,
    reservation_id: String,
    amount_micros: i64,
    settled: bool,
    state: &'static str,
    overrun: bool,
}

pub async fn reserve(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ReserveBody>,
) -> Result<Json<Value>, ApiError> {
    let services = state.services()?;
    if body.max_cost_micros <= 0 {
        return Err(ApiError(PairError::new(
            ErrorCode::InvalidInput,
            "max_cost_micros must be positive",
        )));
    }
    let hold = adapter_budget::hold(
        &services.registry,
        body.model_id.as_deref(),
        &body.price_version,
        services.adapter_budget,
        Micros(body.max_cost_micros),
    )?;
    let task = TaskId(body.task_id);
    let id = services
        .budget
        .reserve_for_model(
            ReserveRequest {
                task,
                max_cost: hold,
                kind: body.kind,
                category: body.category,
                price_version: Some(body.price_version.clone()),
            },
            body.model_id.as_deref(),
        )
        .await?;
    ok(ReserveResponse {
        reservation_id: id.to_string(),
        task_id: task.to_string(),
        model_id: body.model_id,
        kind: body.kind,
        category: body.category,
        price_version: body.price_version,
        max_cost_micros: hold.0,
    })
}

pub async fn reconcile(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ReconcileBody>,
) -> Result<Json<Value>, ApiError> {
    let services = state.services()?;
    let reservation = ReservationId(body.reservation_id);
    let binding = services.budget.reservation_binding(reservation).await?;
    if binding.task.0 != body.task_id {
        return Err(ApiError(PairError::new(
            ErrorCode::PolicyDenied,
            "reservation belongs to a different task",
        )));
    }
    let actual_cost = match body.actual_cost_micros {
        Some(reported) => Some(adapter_budget::settlement(
            &services.registry,
            binding.model_id.as_deref(),
            &binding.price_version,
            services.adapter_budget,
            binding.reserved,
            (body.input_tokens, body.output_tokens, Micros(reported)),
        )?),
        None => None,
    };
    let settled = services
        .budget
        .reconcile_detailed(
            reservation,
            UsageReport {
                input_tokens: body.input_tokens,
                output_tokens: body.output_tokens,
                actual_cost,
                price_version: body.price_version,
            },
        )
        .await?;
    ok(ReconcileResponse {
        entry_id: settled.entry.id.to_string(),
        reservation_id: settled.entry.reservation.to_string(),
        amount_micros: settled.entry.amount.0,
        settled: settled.entry.settled,
        state: if settled.entry.settled {
            STATE_SETTLED
        } else {
            STATE_UNRESOLVED
        },
        overrun: settled.overrun,
    })
}
