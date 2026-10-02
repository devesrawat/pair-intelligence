//! Model-call ledger rows and audit events.
use super::{db_err, ConversationStore};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, ModelCallId, ReservationId, TaskId, TraceId};
use pair_core::money::Micros;
use pair_core::types::{ModelRequest, ModelResponse};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostState {
    /// No price known; nothing was charged by PAIR's accounting.
    Unpriced,
    /// Cost computed from reported usage; awaiting budget reconciliation.
    Priced,
    /// Settled in the budget ledger.
    Reconciled,
}

impl CostState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unpriced => "unpriced",
            Self::Priced => "priced",
            Self::Reconciled => "reconciled",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "unpriced" => Ok(Self::Unpriced),
            "priced" => Ok(Self::Priced),
            "reconciled" => Ok(Self::Reconciled),
            other => Err(PairError::new(ErrorCode::Internal, format!("corrupt cost_state {other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    Ok,
    Error,
}

impl CallStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelCallRecord {
    pub id: ModelCallId,
    pub conversation: Option<ConversationId>,
    pub response_message_id: Option<Uuid>,
    pub task: TaskId,
    pub trace: TraceId,
    pub reservation: Option<ReservationId>,
    pub provider: String,
    pub requested_model: String,
    pub resolved_model: Option<String>,
    pub provider_request_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: Option<Micros>,
    pub price_version: Option<String>,
    pub cost_state: CostState,
    pub route_reason: String,
    pub latency_ms: u64,
    pub status: CallStatus,
    pub error_code: Option<String>,
    pub verification: String,
}

impl ModelCallRecord {
    /// Record for a successful call; the RESOLVED model id from the provider is stored.
    pub fn from_response(
        req: &ModelRequest,
        provider: &str,
        route_reason: &str,
        conversation: Option<ConversationId>,
        resp: &ModelResponse,
    ) -> Self {
        let cost_state = if resp.usage.actual_cost.is_some() { CostState::Priced } else { CostState::Unpriced };
        Self {
            id: ModelCallId::new(),
            conversation,
            response_message_id: None,
            task: req.task,
            trace: req.trace,
            reservation: None,
            provider: provider.to_owned(),
            requested_model: req.model_id.clone(),
            resolved_model: Some(resp.resolved_model.clone()),
            provider_request_id: resp.provider_request_id.clone(),
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            cost: resp.usage.actual_cost,
            price_version: Some(resp.usage.price_version.clone()),
            cost_state,
            route_reason: route_reason.to_owned(),
            latency_ms: resp.latency_ms,
            status: CallStatus::Ok,
            error_code: None,
            verification: "none".to_owned(),
        }
    }

    pub fn from_error(
        req: &ModelRequest,
        provider: &str,
        route_reason: &str,
        conversation: Option<ConversationId>,
        code: ErrorCode,
        latency_ms: u64,
    ) -> Self {
        Self {
            id: ModelCallId::new(),
            conversation,
            response_message_id: None,
            task: req.task,
            trace: req.trace,
            reservation: None,
            provider: provider.to_owned(),
            requested_model: req.model_id.clone(),
            resolved_model: None,
            provider_request_id: None,
            input_tokens: 0,
            output_tokens: 0,
            cost: None,
            price_version: None,
            cost_state: CostState::Unpriced,
            route_reason: route_reason.to_owned(),
            latency_ms,
            status: CallStatus::Error,
            error_code: Some(format!("{code:?}")),
            verification: "none".to_owned(),
        }
    }
}

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| PairError::new(ErrorCode::InvalidInput, format!("{what} out of range")))
}

impl ConversationStore {
    pub async fn record_model_call(&self, r: &ModelCallRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO model_calls (id, conversation_id, response_message_id, task_id, trace_id, reservation_id, \
             provider, requested_model, resolved_model, provider_request_id, input_tokens, output_tokens, \
             cost_micros, price_version, cost_state, route_reason, latency_ms, status, error_code, verification) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)",
        )
        .bind(r.id.0)
        .bind(r.conversation.map(|c| c.0))
        .bind(r.response_message_id)
        .bind(r.task.0)
        .bind(r.trace.0)
        .bind(r.reservation.map(|x| x.0))
        .bind(&r.provider)
        .bind(&r.requested_model)
        .bind(&r.resolved_model)
        .bind(&r.provider_request_id)
        .bind(to_i64(r.input_tokens, "input_tokens")?)
        .bind(to_i64(r.output_tokens, "output_tokens")?)
        .bind(r.cost.map(|c| c.0))
        .bind(&r.price_version)
        .bind(r.cost_state.as_str())
        .bind(&r.route_reason)
        .bind(to_i64(r.latency_ms, "latency_ms")?)
        .bind(r.status.as_str())
        .bind(&r.error_code)
        .bind(&r.verification)
        .execute(&self.pool)
        .await
        .map_err(|e| db_err(&e))?;
        Ok(())
    }

    pub async fn get_model_call(&self, id: ModelCallId) -> Result<ModelCallRecord> {
        let row = sqlx::query("SELECT * FROM model_calls WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| db_err(&e))?
            .ok_or_else(|| PairError::new(ErrorCode::NotFound, format!("model call {id} not found")))?;
        let g = |e: sqlx::Error| db_err(&e);
        let cost_state: String = row.try_get("cost_state").map_err(g)?;
        let status: String = row.try_get("status").map_err(g)?;
        let input: i64 = row.try_get("input_tokens").map_err(g)?;
        let output: i64 = row.try_get("output_tokens").map_err(g)?;
        let latency: i64 = row.try_get("latency_ms").map_err(g)?;
        Ok(ModelCallRecord {
            id,
            conversation: row.try_get::<Option<Uuid>, _>("conversation_id").map_err(g)?.map(ConversationId),
            response_message_id: row.try_get("response_message_id").map_err(g)?,
            task: TaskId(row.try_get("task_id").map_err(g)?),
            trace: TraceId(row.try_get("trace_id").map_err(g)?),
            reservation: row.try_get::<Option<Uuid>, _>("reservation_id").map_err(g)?.map(ReservationId),
            provider: row.try_get("provider").map_err(g)?,
            requested_model: row.try_get("requested_model").map_err(g)?,
            resolved_model: row.try_get("resolved_model").map_err(g)?,
            provider_request_id: row.try_get("provider_request_id").map_err(g)?,
            input_tokens: u64::try_from(input).unwrap_or(0),
            output_tokens: u64::try_from(output).unwrap_or(0),
            cost: row.try_get::<Option<i64>, _>("cost_micros").map_err(g)?.map(Micros),
            price_version: row.try_get("price_version").map_err(g)?,
            cost_state: CostState::parse(&cost_state)?,
            route_reason: row.try_get("route_reason").map_err(g)?,
            latency_ms: u64::try_from(latency).unwrap_or(0),
            status: if status == "ok" { CallStatus::Ok } else { CallStatus::Error },
            error_code: row.try_get("error_code").map_err(g)?,
            verification: row.try_get("verification").map_err(g)?,
        })
    }

    /// Mark a call as settled in the budget ledger.
    pub async fn mark_reconciled(&self, id: ModelCallId, reservation: ReservationId) -> Result<()> {
        let n = sqlx::query("UPDATE model_calls SET cost_state = 'reconciled', reservation_id = $2 WHERE id = $1")
            .bind(id.0)
            .bind(reservation.0)
            .execute(&self.pool)
            .await
            .map_err(|e| db_err(&e))?
            .rows_affected();
        if n == 0 {
            return Err(PairError::new(ErrorCode::NotFound, format!("model call {id} not found")));
        }
        Ok(())
    }

    pub async fn set_verification(&self, id: ModelCallId, verification: &str) -> Result<()> {
        sqlx::query("UPDATE model_calls SET verification = $2 WHERE id = $1")
            .bind(id.0)
            .bind(verification)
            .execute(&self.pool)
            .await
            .map_err(|e| db_err(&e))?;
        Ok(())
    }

    pub async fn record_audit_event(&self, trace: TraceId, actor: &str, kind: &str, subject: &str, detail: &Value) -> Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO audit_events (id, trace_id, actor, kind, subject, detail) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(id)
            .bind(trace.0)
            .bind(actor)
            .bind(kind)
            .bind(subject)
            .bind(detail)
            .execute(&self.pool)
            .await
            .map_err(|e| db_err(&e))?;
        Ok(id)
    }
}
