//! Budget-wrapped provider call shared by the coding and research workflows.
use pair_core::{
    error::Result,
    money::Micros,
    traits::{Budget, Provider},
    types::{ModelRequest, ModelResponse, UsageReport},
};

/// reserve -> generate -> reconcile. A failed call reconciles zero usage so the
/// reservation never leaks.
pub async fn budgeted_generate(
    provider: &dyn Provider,
    budget: &dyn Budget,
    req: ModelRequest,
    max_cost: Micros,
) -> Result<ModelResponse> {
    let reservation = budget.reserve(req.task, max_cost).await?;
    match provider.generate(req).await {
        Ok(resp) => {
            budget.reconcile(reservation, resp.usage.clone()).await?;
            Ok(resp)
        }
        Err(e) => {
            let zero = UsageReport {
                input_tokens: 0,
                output_tokens: 0,
                actual_cost: Some(Micros::ZERO),
                price_version: String::new(),
            };
            if let Err(re) = budget.reconcile(reservation, zero).await {
                tracing::error!(error = %re, "reservation release failed after provider error");
            }
            Err(e)
        }
    }
}
