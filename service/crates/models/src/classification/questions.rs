//! Loads `config/jev-questions.json` and compiles it into Jev Choice questions.
use pair_core::error::{ErrorCode, PairError, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub const INTENT_ID: &str = "intent";
pub const DIFFICULTY_ID: &str = "difficulty";
pub const INTENT_LABELS: [&str; 7] =
    ["coding", "research", "planning", "memory_recall", "transformation", "mixed", "uncertain"];
pub const DIFFICULTY_LABELS: [&str; 4] = ["routine", "substantial", "deep", "uncertain"];

#[derive(Debug, Clone, Deserialize)]
pub struct LabelSpec {
    pub description: String,
    pub positive_examples: Vec<String>,
    pub boundary_cases: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionSpec {
    pub instructions: String,
    pub labels: BTreeMap<String, LabelSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionSet {
    pub question_version: String,
    pub questions: BTreeMap<String, QuestionSpec>,
}

fn same_labels(spec: &QuestionSpec, expected: &[&str]) -> bool {
    spec.labels.len() == expected.len() && expected.iter().all(|l| spec.labels.contains_key(*l))
}

impl QuestionSet {
    pub fn from_json(text: &str) -> Result<Self> {
        let set: Self = serde_json::from_str(text)
            .map_err(|e| PairError::new(ErrorCode::InvalidInput, format!("jev questions: {e}")))?;
        let intent_ok = set.questions.get(INTENT_ID).is_some_and(|q| same_labels(q, &INTENT_LABELS));
        let diff_ok = set.questions.get(DIFFICULTY_ID).is_some_and(|q| same_labels(q, &DIFFICULTY_LABELS));
        if !intent_ok || !diff_ok || set.question_version.is_empty() {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "jev questions must define intent and difficulty with exactly the spec labels and a questionVersion",
            ));
        }
        Ok(set)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| PairError::new(ErrorCode::InvalidInput, format!("read {}: {e}", path.display())))?;
        Self::from_json(&text)
    }

    pub fn labels(&self, id: &str) -> Vec<String> {
        self.questions.get(id).map(|q| q.labels.keys().cloned().collect()).unwrap_or_default()
    }

    /// Wire form: `{id: {type: "choice", instructions, criteria: {label: {...}}}}`.
    pub fn to_wire(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (id, q) in &self.questions {
            let criteria: serde_json::Map<String, Value> = q
                .labels
                .iter()
                .map(|(label, spec)| {
                    (
                        label.clone(),
                        json!({
                            "description": spec.description,
                            "positive_examples": spec.positive_examples,
                            "boundary_cases": spec.boundary_cases,
                        }),
                    )
                })
                .collect();
            out.insert(id.clone(), json!({"type": "choice", "instructions": q.instructions, "criteria": criteria}));
        }
        Value::Object(out)
    }

    /// Serialised size of the question block, used for worst-case token estimation.
    pub fn wire_chars(&self) -> usize {
        self.to_wire().to_string().chars().count()
    }
}

#[cfg(test)]
pub(crate) fn test_questions() -> QuestionSet {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/jev-questions.json");
    QuestionSet::from_path(&path).expect("jev-questions.json loads")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_wire_shipped_questions_have_descriptions_examples_and_boundaries() {
        let set = test_questions();
        let wire = set.to_wire();
        let coding = &wire["intent"]["criteria"]["coding"];
        assert_eq!(wire["intent"]["type"], "choice");
        assert!(coding["description"].is_string());
        assert!(!coding["positive_examples"].as_array().expect("array").is_empty());
        assert!(!coding["boundary_cases"].as_array().expect("array").is_empty());
        assert!(!set.question_version.is_empty());
    }

    #[test]
    fn test_from_json_missing_label_rejected() {
        let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/jev-questions.json"))
            .expect("read");
        let mut v: Value = serde_json::from_str(&text).expect("json");
        v["questions"]["intent"]["labels"].as_object_mut().expect("obj").remove("mixed");
        assert!(QuestionSet::from_json(&v.to_string()).is_err());
    }
}
