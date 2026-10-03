//! Budget-wrapped provider calls shared by the coding and research workflows.
//!
//! The reservation is derived from the registry price and the request itself, never from a
//! caller-supplied figure. A failed provider call leaves its reservation UNRESOLVED (actual
//! cost unknown, full reservation stays counted); it is never reconciled at zero cost.
use crate::limits::RunLimits;
use pair_context::wrap_external;
use pair_core::{
    error::{ErrorCode, PairError, Result},
    money::{Micros, Price},
    traits::{BudgetEx, Provider},
    types::{
        DataClass, ModelMessage, ModelRequest, ModelResponse, ReserveRequest, TaskKind,
        TaskProfile, TrustClass, UsageReport,
    },
};
use pair_models::{
    provider::ProviderRegistry,
    router::{AttemptLimiter, ConfigRouter},
};

/// Content from anyone but the owner (page text, model output, diffs, tool output) reaches a
/// model only as a delimited, labelled data block that cannot forge its own delimiters.
/// `TrustClass::Owner` content is not external and must not go through here.
pub fn data_message(source: &str, trust: TrustClass, content: &str) -> ModelMessage {
    ModelMessage {
        role: "user".into(),
        content: wrap_external(source, trust, content),
        trust,
    }
}

/// Maximum model attempts (generation, repair, escalation) for one logical call (spec 6).
pub const MAX_MODEL_ATTEMPTS: usize = 3;
const PROFILE_DIFFICULTY: &str = "substantial";

/// The router's ordered model choices for a task profile. Hard constraints (provider, data
/// class, capability, budget) are the router's job; this crate never picks a model itself.
pub trait ModelPlanner: Send + Sync {
    fn attempt_order(&self, profile: &TaskProfile) -> Result<Vec<String>>;
}

impl ModelPlanner for ConfigRouter {
    fn attempt_order(&self, profile: &TaskProfile) -> Result<Vec<String>> {
        Ok(self
            .plan(profile, None, &self.default_constraints())?
            .attempt_order())
    }
}

/// Every model call of one run: router plan -> attempt limiter -> deadline -> budget.
pub struct ModelCaller<'a> {
    pub provider: &'a dyn Provider,
    pub budget: &'a dyn BudgetEx,
    pub prices: &'a dyn PriceSource,
    pub planner: &'a dyn ModelPlanner,
    pub limits: &'a RunLimits,
    pub kind: TaskKind,
    /// Router intent of the workflow (`coding`, `research`).
    pub intent: &'static str,
}

impl ModelCaller<'_> {
    /// Tries the router's models in order, at most [`MAX_MODEL_ATTEMPTS`] attempts in total.
    /// Only an unavailable or timed-out provider moves on to the next model; every other
    /// error (budget, policy, bad request) stops immediately.
    pub async fn generate(&self, req: ModelRequest) -> Result<ModelResponse> {
        let profile = TaskProfile {
            intent: self.intent.to_string(),
            difficulty: PROFILE_DIFFICULTY.to_string(),
            data_class: req.data_class,
            needs_tools: false,
            est_input_tokens: estimate_input_tokens(&req),
        };
        let mut attempts = AttemptLimiter::new(MAX_MODEL_ATTEMPTS);
        let mut last = PairError::new(ErrorCode::ProviderUnavailable, "router offered no model");
        for model_id in self.planner.attempt_order(&profile)? {
            attempts = attempts.begin_attempt()?;
            let left = self.limits.remaining()?;
            let mut attempt = req.clone();
            attempt.model_id = model_id;
            attempt.deadline_ms = req
                .deadline_ms
                .min(u64::try_from(left.as_millis()).unwrap_or(u64::MAX));
            match budgeted_generate(self.provider, self.budget, self.prices, self.kind, attempt)
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(e)
                    if matches!(
                        e.code,
                        ErrorCode::ProviderUnavailable | ErrorCode::ProviderTimeout
                    ) =>
                {
                    tracing::warn!(error = %e, "model attempt failed; trying the router's next model");
                    last = e;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

/// What the workflows need to know about a model before spending money on it.
#[derive(Debug, Clone)]
pub struct ModelTerms {
    /// `None`: unknown price, paid execution is disabled.
    pub price: Option<Price>,
    pub allowed_data_classes: Vec<DataClass>,
}

/// Source of model terms (the provider registry in production).
pub trait PriceSource: Send + Sync {
    fn terms(&self, model_id: &str) -> Option<ModelTerms>;
}

impl PriceSource for ProviderRegistry {
    fn terms(&self, model_id: &str) -> Option<ModelTerms> {
        self.get(model_id).map(|e| ModelTerms {
            price: e.price.clone(),
            allowed_data_classes: e.allowed_data_classes.clone(),
        })
    }
}

/// Worst-case input token estimate: one token per byte never under-estimates.
fn estimate_input_tokens(req: &ModelRequest) -> u64 {
    req.messages
        .iter()
        .map(|m| u64::try_from(m.content.len()).unwrap_or(u64::MAX))
        .fold(0u64, u64::saturating_add)
}

/// Reservation = registry price x (estimated input + max output tokens), rounded up.
pub fn reservation_cost(price: &Price, req: &ModelRequest) -> Result<Micros> {
    price
        .max_cost(estimate_input_tokens(req), u64::from(req.max_output_tokens))
        .ok_or_else(|| PairError::new(ErrorCode::BudgetUnknownPrice, "reservation overflows"))
}

/// reserve -> generate -> reconcile.
pub async fn budgeted_generate(
    provider: &dyn Provider,
    budget: &dyn BudgetEx,
    prices: &dyn PriceSource,
    kind: TaskKind,
    req: ModelRequest,
) -> Result<ModelResponse> {
    let terms = prices.terms(&req.model_id).ok_or_else(|| {
        PairError::new(
            ErrorCode::InvalidInput,
            format!("model {:?} is not in the registry", req.model_id),
        )
    })?;
    if !terms.allowed_data_classes.contains(&req.data_class) {
        return Err(PairError::new(
            ErrorCode::ProviderDisallowed,
            format!(
                "model {:?} is not allowed to see {:?} data",
                req.model_id, req.data_class
            ),
        ));
    }
    let price = terms.price.ok_or_else(|| {
        PairError::new(
            ErrorCode::BudgetUnknownPrice,
            format!(
                "model {:?} has no price; paid execution disabled",
                req.model_id
            ),
        )
    })?;
    let max_cost = reservation_cost(&price, &req)?;
    let reservation = budget
        .reserve_with(ReserveRequest::metered(
            req.task,
            max_cost,
            kind,
            price.version.clone(),
        ))
        .await?;
    match provider.generate(req).await {
        Ok(resp) => {
            budget.reconcile(reservation, resp.usage.clone()).await?;
            Ok(resp)
        }
        Err(e) => {
            // Actual cost is unknown (the provider may have billed a partial call): record
            // that and keep the whole reservation counted instead of assuming zero.
            let unknown = UsageReport {
                input_tokens: 0,
                output_tokens: 0,
                actual_cost: None,
                price_version: price.version,
            };
            if let Err(re) = budget.reconcile(reservation, unknown).await {
                tracing::error!(error = %re, "could not mark reservation unresolved after provider error");
            }
            Err(e)
        }
    }
}
