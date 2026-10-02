use serde::{Deserialize, Serialize};

/// Currency in integer micro-USD (1 USD = 1_000_000). Never floating point.
pub const MICROS_PER_USD: i64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Micros(pub i64);

impl Micros {
    pub const ZERO: Micros = Micros(0);
    pub const fn usd_cents(cents: i64) -> Self {
        Micros(cents * 10_000)
    }
    pub fn checked_add(self, o: Micros) -> Option<Micros> {
        self.0.checked_add(o.0).map(Micros)
    }
    pub fn checked_sub(self, o: Micros) -> Option<Micros> {
        self.0.checked_sub(o.0).map(Micros)
    }
}

/// Price per million tokens, in micro-USD, tied to a price version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    pub version: String,
    pub input_per_mtok: Micros,
    pub output_per_mtok: Micros,
}

impl Price {
    /// Worst-case cost, rounded up so reservations never under-estimate.
    pub fn max_cost(&self, input_tokens: u64, max_output_tokens: u64) -> Option<Micros> {
        let i = ceil_div(
            input_tokens.checked_mul(self.input_per_mtok.0 as u64)?,
            1_000_000,
        );
        let o = ceil_div(
            max_output_tokens.checked_mul(self.output_per_mtok.0 as u64)?,
            1_000_000,
        );
        i.checked_add(o)
            .and_then(|v| i64::try_from(v).ok())
            .map(Micros)
    }
}

fn ceil_div(a: u64, b: u64) -> u64 {
    a.div_ceil(b)
}
