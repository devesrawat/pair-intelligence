//! `POST /v1/budget/reserve` and `POST /v1/budget/reconcile` (OpenClaw adapter).
//!
//! The adapter reserves in `before_model_resolve` from its own estimate and settles from
//! `llm_output.usage`. Kind, category and price version are always explicit: nothing is inferred.

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
use crate::error::ApiError;
use crate::json::ApiJson;
use crate::state::AppState;

const STATE_SETTLED: &str = "settled";
const STATE_UNRESOLVED: &str = "unresolved";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveBody {
    /// Omitted: a fresh task id is generated and returned.
    pub task_id: Option<Uuid>,
    pub kind: TaskKind,
    pub category: BudgetCategory,
    pub max_cost_micros: i64,
    pub price_version: String,
}

#[derive(Debug, Serialize)]
struct ReserveResponse {
    reservation_id: String,
    task_id: String,
    kind: TaskKind,
    category: BudgetCategory,
    price_version: String,
    max_cost_micros: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileBody {
    pub reservation_id: Uuid,
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
    let task = body.task_id.map_or_else(TaskId::new, TaskId);
    let id = services
        .budget
        .reserve_with(ReserveRequest {
            task,
            max_cost: Micros(body.max_cost_micros),
            kind: body.kind,
            category: body.category,
            price_version: Some(body.price_version.clone()),
        })
        .await?;
    ok(ReserveResponse {
        reservation_id: id.to_string(),
        task_id: task.to_string(),
        kind: body.kind,
        category: body.category,
        price_version: body.price_version,
        max_cost_micros: body.max_cost_micros,
    })
}

pub async fn reconcile(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<ReconcileBody>,
) -> Result<Json<Value>, ApiError> {
    let services = state.services()?;
    let settled = services
        .budget
        .reconcile_detailed(
            ReservationId(body.reservation_id),
            UsageReport {
                input_tokens: body.input_tokens,
                output_tokens: body.output_tokens,
                actual_cost: body.actual_cost_micros.map(Micros),
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
