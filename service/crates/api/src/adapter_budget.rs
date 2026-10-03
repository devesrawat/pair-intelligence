//! Server-side cost rules for the adapter budget routes. The adapter's own numbers are inputs, never
//! authority: the hold is at least the registry worst case, a settlement is at least tokens x
//! registry price, and no settlement may claim more than a bounded multiple of what was held.

use pair_budget::MAX_COST_MICROS;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::money::Micros;
use pair_models::provider::{ModelEntry, ProviderRegistry};

/// A settlement may claim at most `reserved x OVERRUN_FACTOR` (default).
pub const DEFAULT_OVERRUN_FACTOR: u32 = 4;
/// Input tokens the server assumes for one call when sizing a hold (default). The adapter knows its
/// real prompt; the server only knows a floor, which the client can raise but never lower.
pub const DEFAULT_RESERVE_FLOOR_INPUT_TOKENS: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterBudget {
    pub overrun_factor: u32,
    pub reserve_floor_input_tokens: u64,
}

impl Default for AdapterBudget {
    fn default() -> Self {
        Self {
            overrun_factor: DEFAULT_OVERRUN_FACTOR,
            reserve_floor_input_tokens: DEFAULT_RESERVE_FLOOR_INPUT_TOKENS,
        }
    }
}

fn unknown_price(what: String) -> PairError {
    PairError::new(ErrorCode::BudgetUnknownPrice, what)
}

fn invalid(what: String) -> PairError {
    PairError::new(ErrorCode::InvalidInput, what)
}

/// Entries priced under `version`, restricted to `model` when one is named.
fn priced<'a>(
    registry: &'a ProviderRegistry,
    model: Option<&str>,
    version: &str,
) -> Result<Vec<&'a ModelEntry>> {
    let entries: Vec<&ModelEntry> = match model {
        Some(id) => {
            let entry = registry
                .get(id)
                .ok_or_else(|| unknown_price(format!("model {id:?} is not in the registry")))?;
            vec![entry]
        }
        None => registry.iter().collect(),
    };
    let matching: Vec<&ModelEntry> = entries
        .into_iter()
        .filter(|e| e.price.as_ref().is_some_and(|p| p.version == version))
        .collect();
    if matching.is_empty() {
        return Err(unknown_price(match model {
            Some(id) => format!("model {id:?} has no price under version {version:?}"),
            None => format!("no registry model is priced under version {version:?}"),
        }));
    }
    Ok(matching)
}

fn entry_worst_case(entry: &ModelEntry, floor_input_tokens: u64) -> Result<Micros> {
    let price = entry
        .price
        .as_ref()
        .ok_or_else(|| unknown_price(format!("model {:?} has no price", entry.id)))?;
    price
        .max_cost(
            floor_input_tokens.min(entry.context_tokens),
            entry.max_output_tokens,
        )
        .ok_or_else(|| invalid(format!("worst-case cost of {:?} overflows", entry.id)))
}

/// The hold for a reservation: the larger of the client's figure and the registry worst case for
/// the named model (or, with no model, for the dearest model priced under the version).
pub fn hold(
    registry: &ProviderRegistry,
    model: Option<&str>,
    version: &str,
    limits: AdapterBudget,
    client_max: Micros,
) -> Result<Micros> {
    let mut worst = Micros::ZERO;
    for entry in priced(registry, model, version)? {
        worst = worst.max(entry_worst_case(entry, limits.reserve_floor_input_tokens)?);
    }
    Ok(worst.max(client_max))
}

/// tokens x registry price for the reserved model and price version; with no (or a vanished) model,
/// the dearest model priced under the version, so an unnamed model is never the cheap way out.
fn token_cost(
    registry: &ProviderRegistry,
    model: Option<&str>,
    version: &str,
    tokens: (u64, u64),
) -> Result<Micros> {
    let named = model.filter(|id| registry.get(id).is_some());
    let mut cost = Micros::ZERO;
    for entry in priced(registry, named, version)? {
        let price = entry
            .price
            .as_ref()
            .ok_or_else(|| unknown_price(format!("model {:?} has no price", entry.id)))?;
        let this = price
            .max_cost(tokens.0, tokens.1)
            .ok_or_else(|| invalid("token counts overflow the cost".to_owned()))?;
        cost = cost.max(this);
    }
    Ok(cost)
}

/// What a settlement counts: `max(reported, tokens x registry price)`. Refused when either exceeds
/// `reserved x overrun_factor` or the hard ceiling (such a report is not an overrun to absorb, it
/// is nonsense; the caller may report `null` and leave the reservation unresolved).
pub fn settlement(
    registry: &ProviderRegistry,
    model: Option<&str>,
    version: &str,
    limits: AdapterBudget,
    reserved: Micros,
    usage: (u64, u64, Micros),
) -> Result<Micros> {
    let (input, output, reported) = usage;
    let ceiling = reserved
        .0
        .saturating_mul(i64::from(limits.overrun_factor))
        .min(MAX_COST_MICROS);
    let counted = reported.max(token_cost(registry, model, version, (input, output))?);
    if counted.0 > ceiling {
        return Err(invalid(format!(
            "settlement of {} micro-USD exceeds {} x the {} reserved (limit {ceiling}); \
             report a null cost to leave the reservation unresolved",
            counted.0, limits.overrun_factor, reserved.0
        )));
    }
    Ok(counted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pair_core::money::Price;
    use pair_core::types::DataClass;
    use pair_models::provider::{Billing, Endpoint, Health, ProviderKind};

    const VERSION: &str = "pv";

    fn entry(id: &str, input: i64, output: i64) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            upstream_id: id.to_owned(),
            billing: Billing::Metered,
            provider: ProviderKind::Anthropic,
            endpoint: Endpoint::unchecked_for_tests("http://127.0.0.1:1"),
            modalities: vec!["text".to_owned()],
            context_tokens: 100_000,
            max_output_tokens: 1_000,
            tools: false,
            structured_output: false,
            price: Some(Price {
                version: VERSION.to_owned(),
                input_per_mtok: Micros(input),
                output_per_mtok: Micros(output),
            }),
            data_policy: "test".to_owned(),
            allowed_data_classes: vec![DataClass::Public],
            quota_requests_per_minute: None,
            health: Health::Healthy,
            id_verified: true,
        }
    }

    fn registry() -> ProviderRegistry {
        ProviderRegistry::from_entries(vec![
            entry("cheap", 1_000_000, 5_000_000),
            entry("dear", 4_000_000, 20_000_000),
        ])
        .expect("registry")
    }

    const LIMITS: AdapterBudget = AdapterBudget {
        overrun_factor: 4,
        reserve_floor_input_tokens: 10_000,
    };

    #[test]
    fn test_hold_client_figure_can_only_raise_the_registry_worst_case() {
        let r = registry();
        // cheap: 10_000 x $1/M + 1_000 x $5/M = 10_000 + 5_000.
        let floor = hold(&r, Some("cheap"), VERSION, LIMITS, Micros(1)).expect("hold");
        assert_eq!(floor, Micros(15_000));
        let raised = hold(&r, Some("cheap"), VERSION, LIMITS, Micros(99_000)).expect("hold");
        assert_eq!(raised, Micros(99_000));
        let unnamed = hold(&r, None, VERSION, LIMITS, Micros(1)).expect("hold");
        assert_eq!(unnamed, Micros(60_000), "the dearest model sets the floor");
    }

    #[test]
    fn test_hold_unknown_model_or_version_is_unknown_price() {
        let r = registry();
        for (model, version) in [
            (Some("nope"), VERSION),
            (Some("cheap"), "other"),
            (None, "x"),
        ] {
            let err = hold(&r, model, version, LIMITS, Micros(1)).expect_err("refused");
            assert_eq!(err.code, ErrorCode::BudgetUnknownPrice);
        }
    }

    #[test]
    fn test_settlement_counts_the_larger_of_reported_and_token_cost() {
        let r = registry();
        // 100_000 in x $1/M + 10_000 out x $5/M = 150_000.
        let under = settlement(
            &r,
            Some("cheap"),
            VERSION,
            LIMITS,
            Micros(100_000),
            (100_000, 10_000, Micros(1)),
        )
        .expect("settles");
        assert_eq!(under, Micros(150_000));
        let over = settlement(
            &r,
            Some("cheap"),
            VERSION,
            LIMITS,
            Micros(100_000),
            (1, 1, Micros(200_000)),
        )
        .expect("settles");
        assert_eq!(over, Micros(200_000));
    }

    #[test]
    fn test_settlement_refuses_beyond_overrun_factor_and_hard_ceiling() {
        let r = registry();
        for reported in [400_001, i64::MAX, MAX_COST_MICROS + 1] {
            let err = settlement(
                &r,
                Some("cheap"),
                VERSION,
                LIMITS,
                Micros(100_000),
                (1, 1, Micros(reported)),
            )
            .expect_err("refused");
            assert_eq!(err.code, ErrorCode::InvalidInput, "{reported}");
        }
        // Tokens that price above the bound are refused too, and overflowing counts do not panic.
        let err = settlement(
            &r,
            Some("cheap"),
            VERSION,
            LIMITS,
            Micros(100),
            (u64::MAX, u64::MAX, Micros(1)),
        )
        .expect_err("refused");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn test_settlement_without_a_model_prices_at_the_dearest_model() {
        let r = registry();
        // 100_000 in x $4/M + 10_000 out x $20/M = 600_000.
        let cost = settlement(
            &r,
            None,
            VERSION,
            LIMITS,
            Micros(200_000),
            (100_000, 10_000, Micros(1)),
        )
        .expect("settles");
        assert_eq!(cost, Micros(600_000));
    }
}
