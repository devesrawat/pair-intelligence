//! Jev response wire types and adapter-boundary validation.
//! Field names follow https://docs.typesafe.ai/api (read 2026-10-03). Treat as UNVERIFIED against a
//! live call until the owner's key is used; `usage` is accepted as `input_tokens` or `input`.
use pair_core::error::{ErrorCode, PairError, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

const PROB_SUM_TOLERANCE: f64 = 0.05;
const TOP_TIE_EPSILON: f64 = 1e-9;

#[derive(Debug, Deserialize)]
pub struct WireResponse {
    pub model: String,
    pub answers: BTreeMap<String, WireAnswer>,
    pub usage: WireUsage,
}

#[derive(Debug, Deserialize)]
pub struct WireAnswer {
    #[serde(rename = "type")]
    pub kind: String,
    pub choice: String,
    pub probabilities: BTreeMap<String, f64>,
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct WireUsage {
    #[serde(alias = "input")]
    pub input_tokens: u64,
    #[serde(default, alias = "output")]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedChoice {
    pub label: String,
    pub probabilities: BTreeMap<String, f64>,
    /// PAIR convention: probability of the chosen (top) option. Not a vendor statistic.
    pub confidence: f64,
}

fn invalid(msg: impl Into<String>) -> PairError {
    PairError::new(ErrorCode::ClassifierInvalid, msg)
}

fn in_unit_range(v: f64) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}

/// Validate labels and numeric ranges for one Choice answer against the configured label set.
pub fn validate_choice(
    question: &str,
    answer: &WireAnswer,
    labels: &[String],
) -> Result<ValidatedChoice> {
    if answer.kind != "choice" {
        return Err(invalid(format!(
            "{question}: answer type {:?} is not choice",
            answer.kind
        )));
    }
    if !labels.contains(&answer.choice) {
        return Err(invalid(format!(
            "{question}: label {:?} is not allowed",
            answer.choice
        )));
    }
    let keys_match = answer.probabilities.len() == labels.len()
        && labels.iter().all(|l| answer.probabilities.contains_key(l));
    if !keys_match {
        return Err(invalid(format!(
            "{question}: probability keys do not equal the label set"
        )));
    }
    if !answer.probabilities.values().all(|p| in_unit_range(*p)) {
        return Err(invalid(format!("{question}: probability outside [0,1]")));
    }
    if answer.confidence.is_some_and(|c| !in_unit_range(c)) {
        return Err(invalid(format!(
            "{question}: vendor confidence outside [0,1]"
        )));
    }
    let sum: f64 = answer.probabilities.values().sum();
    if (sum - 1.0).abs() > PROB_SUM_TOLERANCE {
        return Err(invalid(format!("{question}: probabilities sum to {sum}")));
    }
    let top = answer
        .probabilities
        .values()
        .copied()
        .fold(0.0_f64, f64::max);
    let chosen = answer
        .probabilities
        .get(&answer.choice)
        .copied()
        .unwrap_or(0.0);
    if top - chosen > TOP_TIE_EPSILON {
        return Err(invalid(format!(
            "{question}: chosen label is not the top option"
        )));
    }
    Ok(ValidatedChoice {
        label: answer.choice.clone(),
        probabilities: answer.probabilities.clone(),
        confidence: chosen,
    })
}

pub fn parse_response(body: &str) -> Result<WireResponse> {
    let resp: WireResponse =
        serde_json::from_str(body).map_err(|e| invalid(format!("malformed response: {e}")))?;
    if resp.model.trim().is_empty() {
        return Err(invalid("response model is empty"));
    }
    Ok(resp)
}
