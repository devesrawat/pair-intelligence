//! Budget configuration read from `config/budget.yaml`. Caps are immutable once loaded.
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::money::Micros;
use pair_core::types::TaskKind;
use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;
use std::fmt;
use std::path::Path;

const REQUIRED_TIMEZONE: &str = "Asia/Kolkata";
const REQUIRED_CURRENCY: &str = "USD";
const MICRO_DECIMALS: usize = 6;
const MICROS_PER_UNIT: i64 = 1_000_000;

/// USD amount in YAML (e.g. `20.00`) converted to micros via its decimal text, never float math.
#[derive(Debug, Clone, Copy)]
struct UsdAmount(Micros);

impl<'de> Deserialize<'de> for UsdAmount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = UsdAmount;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a non-negative USD amount with at most 6 decimals")
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<UsdAmount, E> {
                parse_decimal(&v.to_string())
                    .map(UsdAmount)
                    .map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<UsdAmount, E> {
                parse_decimal(&v.to_string())
                    .map(UsdAmount)
                    .map_err(E::custom)
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<UsdAmount, E> {
                parse_decimal(v).map(UsdAmount).map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
}

fn parse_decimal(text: &str) -> std::result::Result<Micros, String> {
    let invalid = || format!("invalid USD amount {text:?}");
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() || frac.len() > MICRO_DECIMALS || !whole.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    let padded = format!("{frac:0<width$}", width = MICRO_DECIMALS);
    let w: i64 = whole.parse().map_err(|_| invalid())?;
    let f: i64 = padded.parse().map_err(|_| invalid())?;
    w.checked_mul(MICROS_PER_UNIT)
        .and_then(|v| v.checked_add(f))
        .map(Micros)
        .ok_or_else(invalid)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    currency: String,
    metered_monthly_cap: UsdAmount,
    metered_daily_cap: UsdAmount,
    classifier_monthly_subcap: UsdAmount,
    default_task_cap: UsdAmount,
    research_task_cap: UsdAmount,
    coding_task_cap: UsdAmount,
    #[serde(default)]
    auto_top_up: bool,
}

/// Other keys of the shipped `schedule:` section; consumed elsewhere, accepted here.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // accepted for strict-key validation only
struct RawSchedule {
    timezone: String,
    #[serde(default)]
    morning: Option<String>,
    #[serde(default)]
    review: Option<String>,
}

/// Unknown keys are rejected so a mistyped cap or `auto_top_up` fails loudly instead of being
/// silently ignored. `execution` belongs to other crates and is accepted unparsed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // `execution` is accepted for strict-key validation only
struct RawFile {
    budget: RawBudget,
    schedule: RawSchedule,
    #[serde(default)]
    execution: Option<serde_yaml_ng::Value>,
}

/// Validated caps. Fields are private and there are no setters: nothing can raise a cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetConfig {
    monthly_cap: Micros,
    daily_cap: Micros,
    classifier_monthly_subcap: Micros,
    default_task_cap: Micros,
    research_task_cap: Micros,
    coding_task_cap: Micros,
}

fn invalid(msg: &str) -> PairError {
    PairError::new(ErrorCode::InvalidInput, format!("budget config: {msg}"))
}

impl BudgetConfig {
    pub fn monthly_cap(&self) -> Micros {
        self.monthly_cap
    }
    pub fn daily_cap(&self) -> Micros {
        self.daily_cap
    }
    pub fn classifier_monthly_subcap(&self) -> Micros {
        self.classifier_monthly_subcap
    }
    pub fn task_cap(&self, kind: TaskKind) -> Micros {
        match kind {
            TaskKind::Default => self.default_task_cap,
            TaskKind::Research => self.research_task_cap,
            TaskKind::Coding => self.coding_task_cap,
        }
    }

    /// Parse and validate keys, currency, timezone and `auto_top_up`. Does NOT check cap ordering
    /// (test fixtures use arbitrary caps); production code must use `load` or `from_yaml_strict`.
    pub fn from_yaml(text: &str) -> Result<Self> {
        let raw: RawFile = serde_yaml_ng::from_str(text).map_err(|e| invalid(&e.to_string()))?;
        let b = raw.budget;
        if b.auto_top_up {
            return Err(invalid("auto_top_up must be false"));
        }
        if b.currency != REQUIRED_CURRENCY {
            return Err(invalid("currency must be USD"));
        }
        if raw.schedule.timezone != REQUIRED_TIMEZONE {
            return Err(invalid("schedule.timezone must be Asia/Kolkata"));
        }
        Ok(Self {
            monthly_cap: b.metered_monthly_cap.0,
            daily_cap: b.metered_daily_cap.0,
            classifier_monthly_subcap: b.classifier_monthly_subcap.0,
            default_task_cap: b.default_task_cap.0,
            research_task_cap: b.research_task_cap.0,
            coding_task_cap: b.coding_task_cap.0,
        })
    }

    /// `from_yaml` plus cap-ordering validation: every task cap <= daily <= monthly, and the
    /// classifier sub-cap <= monthly.
    pub fn from_yaml_strict(text: &str) -> Result<Self> {
        let cfg = Self::from_yaml(text)?;
        cfg.validate_ordering()?;
        Ok(cfg)
    }

    fn validate_ordering(&self) -> Result<()> {
        for (name, cap) in [
            ("default_task_cap", self.default_task_cap),
            ("research_task_cap", self.research_task_cap),
            ("coding_task_cap", self.coding_task_cap),
        ] {
            if cap > self.daily_cap {
                return Err(invalid(&format!("{name} must be <= metered_daily_cap")));
            }
        }
        if self.daily_cap > self.monthly_cap {
            return Err(invalid("metered_daily_cap must be <= metered_monthly_cap"));
        }
        if self.classifier_monthly_subcap > self.monthly_cap {
            return Err(invalid(
                "classifier_monthly_subcap must be <= metered_monthly_cap",
            ));
        }
        Ok(())
    }

    /// Load and strictly validate a config file (the production entry point).
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| invalid(&format!("read {}: {e}", path.display())))?;
        Self::from_yaml_strict(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_decimal_two_places_converts_exactly() {
        assert_eq!(parse_decimal("0.10"), Ok(Micros(100_000)));
        assert_eq!(parse_decimal("20"), Ok(Micros(20_000_000)));
        assert!(parse_decimal("0.1234567").is_err());
        assert!(parse_decimal("-1.0").is_err());
    }
}
