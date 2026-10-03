//! Evaluation harness for the three routing strategies of spec 6.1.
//! Everything computed here is MODELED or measured on DRAFT labels. Nothing in this module is evidence of
//! routing quality: downstream task acceptance requires real workflow runs and owner-reviewed labels.
use super::baseline::classify_by_rules;
use super::config::{ClassifierMode, RoutingConfig};
use crate::router::ConfigRouter;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::TaskId;
use pair_core::traits::Classifier;
use pair_core::types::{ClassificationInput, DataClass, TaskProfile};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Instant;

/// Assumed generation output length for modeled cost. A modeling constant, not a measurement.
pub const ASSUMED_OUTPUT_TOKENS: u64 = 500;
const CHARS_PER_TOKEN: u64 = 4;
const FIXED_BASELINE_INTENT: &str = "__fixed_baseline__";
pub const DRAFT_STATUS: &str = "draft_unreviewed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    Dev,
    HeldOut,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvalCase {
    pub id: String,
    pub text: String,
    pub context: String,
    pub expected_intent: String,
    pub expected_difficulty: String,
    pub expected_workflow: String,
    pub data_class: DataClass,
    pub acceptance_check: String,
    pub split: Split,
    pub label_status: String,
}

pub struct Dataset {
    pub cases: Vec<EvalCase>,
    held_out_lines: Vec<String>,
}

impl Dataset {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            PairError::new(
                ErrorCode::InvalidInput,
                format!("read {}: {e}", path.display()),
            )
        })?;
        let mut cases = Vec::new();
        let mut held_out_lines = Vec::new();
        for (n, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
            let case: EvalCase = serde_json::from_str(line).map_err(|e| {
                PairError::new(
                    ErrorCode::InvalidInput,
                    format!("dataset line {}: {e}", n + 1),
                )
            })?;
            if case.split == Split::HeldOut {
                held_out_lines.push(line.to_string());
            }
            cases.push(case);
        }
        Ok(Self {
            cases,
            held_out_lines,
        })
    }

    pub fn split(&self, split: Split) -> Vec<&EvalCase> {
        self.cases.iter().filter(|c| c.split == split).collect()
    }

    /// SHA-256 over the held-out lines in file order. Compared with `routing.held_out.sha256` to detect edits.
    pub fn held_out_digest(&self) -> String {
        let mut h = Sha256::new();
        for l in &self.held_out_lines {
            h.update(l.as_bytes());
            h.update(b"\n");
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct StrategyReport {
    pub name: String,
    pub split: String,
    pub cases: usize,
    pub intent_correct: Option<usize>,
    pub difficulty_correct: Option<usize>,
    pub modeled_cost_micros: i64,
    pub classifier_cost_micros: i64,
    pub p50_latency_us: u64,
    pub p95_latency_us: u64,
    pub failures: usize,
    pub notes: Vec<String>,
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() * pct)
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[idx]
}

fn est_tokens(case: &EvalCase) -> u64 {
    ((case.text.chars().count() + case.context.chars().count()) as u64).div_ceil(CHARS_PER_TOKEN)
}

fn modeled_cost(router: &ConfigRouter, model_id: &str, input_tokens: u64) -> i64 {
    router
        .config()
        .candidates
        .iter()
        .find(|c| c.id == model_id)
        .map_or(0, |c| {
            let i = input_tokens
                .saturating_mul(c.input_price_micros_per_mtok as u64)
                .div_ceil(1_000_000);
            let o = ASSUMED_OUTPUT_TOKENS
                .saturating_mul(c.output_price_micros_per_mtok as u64)
                .div_ceil(1_000_000);
            i64::try_from(i + o).unwrap_or(i64::MAX)
        })
}

fn profile_for(intent: &str, difficulty: &str, case: &EvalCase) -> TaskProfile {
    TaskProfile {
        intent: intent.into(),
        difficulty: difficulty.into(),
        data_class: case.data_class,
        needs_tools: false,
        est_input_tokens: est_tokens(case),
    }
}

fn finish(mut r: StrategyReport, mut lat: Vec<u64>) -> StrategyReport {
    lat.sort_unstable();
    r.p50_latency_us = percentile(&lat, 50);
    r.p95_latency_us = percentile(&lat, 95);
    r
}

/// Strategy 1: one fixed model tier for every request. It predicts no intent, so accuracy is N/A.
pub fn run_fixed_baseline(
    cases: &[&EvalCase],
    router: &ConfigRouter,
    split: &str,
) -> StrategyReport {
    let mut r = StrategyReport {
        name: "fixed baseline".into(),
        split: split.into(),
        cases: cases.len(),
        ..Default::default()
    };
    let mut lat = Vec::new();
    for case in cases {
        let t = Instant::now();
        let sel = router.select_baseline(&profile_for(FIXED_BASELINE_INTENT, "uncertain", case));
        lat.push(u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX));
        match sel {
            Ok(d) => r.modeled_cost_micros += modeled_cost(router, &d.model_id, est_tokens(case)),
            Err(_) => r.failures += 1,
        }
    }
    finish(r, lat)
}

/// Strategy 2: deterministic rules choose the baseline tier per intent.
pub fn run_rules(cases: &[&EvalCase], router: &ConfigRouter, split: &str) -> StrategyReport {
    let mut r = StrategyReport {
        name: "deterministic rules + baseline".into(),
        split: split.into(),
        cases: cases.len(),
        ..Default::default()
    };
    let (mut ic, mut dc) = (0, 0);
    let mut lat = Vec::new();
    for case in cases {
        let t = Instant::now();
        let rules = classify_by_rules(&case.text);
        let sel = router.select_baseline(&profile_for(rules.intent, rules.difficulty, case));
        lat.push(u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX));
        ic += usize::from(rules.intent == case.expected_intent);
        dc += usize::from(rules.difficulty == case.expected_difficulty);
        match sel {
            Ok(d) => r.modeled_cost_micros += modeled_cost(router, &d.model_id, est_tokens(case)),
            Err(_) => r.failures += 1,
        }
    }
    r.intent_correct = Some(ic);
    r.difficulty_correct = Some(dc);
    finish(r, lat)
}

/// Strategy 3: a classifier recommends the tier (applied as if every category were approved, which is the
/// hypothetical being evaluated). Classifier failures fall back to the baseline and are counted.
pub async fn run_classifier_assisted(
    cases: &[&EvalCase],
    classifier: &dyn Classifier,
    router: &ConfigRouter,
    input_price_micros_per_mtok: i64,
    split: &str,
) -> StrategyReport {
    let mut r = StrategyReport {
        name: "classifier-assisted".into(),
        split: split.into(),
        cases: cases.len(),
        ..Default::default()
    };
    let (mut ic, mut dc) = (0, 0);
    let mut lat = Vec::new();
    for case in cases {
        let rules = classify_by_rules(&case.text);
        let input = ClassificationInput {
            request: case.text.clone(),
            recent_summary: case.context.clone(),
            project_type: None,
            workflows: vec![],
            task: TaskId::new(),
        };
        let started = Instant::now();
        let c = classifier.classify(input).await.ok();
        lat.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
        if c.is_none() {
            r.failures += 1;
        }
        let intent = c.as_ref().map_or(rules.intent, |c| c.intent.as_str());
        ic += usize::from(intent == case.expected_intent);
        dc += usize::from(
            c.as_ref()
                .map_or(rules.difficulty, |c| c.difficulty.as_str())
                == case.expected_difficulty,
        );
        if let Some(c) = &c {
            let spent = (c
                .input_tokens
                .saturating_mul(input_price_micros_per_mtok as u64))
            .div_ceil(1_000_000);
            r.classifier_cost_micros += i64::try_from(spent).unwrap_or(i64::MAX);
        }
        let profile = profile_for(rules.intent, rules.difficulty, case);
        match router.select_hypothetical_active(&profile, c.as_ref()) {
            Ok(d) => r.modeled_cost_micros += modeled_cost(router, &d.model_id, est_tokens(case)),
            Err(_) => r.failures += 1,
        }
    }
    r.intent_correct = Some(ic);
    r.difficulty_correct = Some(dc);
    finish(r, lat)
}

fn pct(n: Option<usize>, total: usize) -> String {
    match n {
        Some(n) if total > 0 => format!("{:.1}% ({n}/{total})", 100.0 * n as f64 / total as f64),
        Some(_) => "n/a".into(),
        None => "N/A (predicts no label)".into(),
    }
}

pub fn render(reports: &[StrategyReport]) -> String {
    let mut out = String::from(
        "NOTICE: labels are draft_unreviewed (owner has not reviewed them). Accuracy below is agreement with DRAFT labels,\n\
         cost is MODELED from placeholder prices (no retries, no acceptance), latency of rules is local compute only.\n\
         Nothing here measures downstream task acceptance or real-world quality.\n\n",
    );
    for r in reports {
        out.push_str(&format!(
            "[{}] {}\n  cases={} intent_acc={} difficulty_acc={}\n  modeled_generation_cost={} micros  classifier_cost={} micros  routing_failures={}\n  latency p50={}us p95={}us\n",
            r.split, r.name, r.cases, pct(r.intent_correct, r.cases), pct(r.difficulty_correct, r.cases),
            r.modeled_cost_micros, r.classifier_cost_micros, r.failures, r.p50_latency_us, r.p95_latency_us,
        ));
        for n in &r.notes {
            out.push_str(&format!("  note: {n}\n"));
        }
    }
    out
}

pub fn load_router(config: &Path) -> Result<ConfigRouter> {
    let cfg = RoutingConfig::from_path(config)?;
    Ok(ConfigRouter::new(cfg).with_mode(ClassifierMode::Disabled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::config::test_config;
    use crate::classification::questions::{DIFFICULTY_LABELS, INTENT_LABELS};
    use async_trait::async_trait;
    use pair_core::types::TaskClassification;

    const DATASET: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../evals/datasets/routing.jsonl"
    );
    const FROZEN_DIGEST_FILE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../evals/datasets/routing.held_out.sha256"
    );
    const CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/models.yaml");

    fn dataset() -> Dataset {
        Dataset::load(Path::new(DATASET)).expect("dataset loads")
    }

    #[test]
    fn test_dataset_distribution_matches_spec_6_1() {
        let d = dataset();
        assert_eq!(d.cases.len(), 100);
        let count = |f: &dyn Fn(&EvalCase) -> bool| d.cases.iter().filter(|c| f(c)).count();
        assert_eq!(count(&|c| c.expected_intent == "coding"), 30);
        assert_eq!(count(&|c| c.expected_intent == "research"), 20);
        assert_eq!(count(&|c| c.expected_intent == "planning"), 15);
        assert_eq!(count(&|c| c.expected_intent == "memory_recall"), 15);
        assert_eq!(count(&|c| c.expected_intent == "transformation"), 10);
        assert_eq!(
            count(&|c| c.expected_intent == "mixed" || c.expected_intent == "uncertain"),
            10
        );
        assert_eq!(d.split(Split::Dev).len(), 60);
        assert_eq!(d.split(Split::HeldOut).len(), 40);
    }

    #[test]
    fn test_dataset_labels_valid_unique_and_marked_draft() {
        let d = dataset();
        let ids: std::collections::HashSet<_> = d.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids.len(), d.cases.len());
        for c in &d.cases {
            assert_eq!(c.label_status, DRAFT_STATUS, "{}", c.id);
            assert!(
                INTENT_LABELS.contains(&c.expected_intent.as_str()),
                "{}",
                c.id
            );
            assert!(
                DIFFICULTY_LABELS.contains(&c.expected_difficulty.as_str()),
                "{}",
                c.id
            );
            assert!(
                !c.acceptance_check.is_empty() && !c.expected_workflow.is_empty(),
                "{}",
                c.id
            );
        }
    }

    #[test]
    fn test_held_out_split_is_frozen() {
        let frozen = std::fs::read_to_string(FROZEN_DIGEST_FILE).expect("digest file");
        assert_eq!(
            dataset().held_out_digest(),
            frozen.trim(),
            "held-out cases changed: this is a new evaluation, not a tweak"
        );
    }

    #[test]
    fn test_run_fixed_and_rules_report_na_for_fixed_and_counts_for_rules() {
        let d = dataset();
        let router = load_router(Path::new(CONFIG)).expect("router");
        let dev = d.split(Split::Dev);
        let fixed = run_fixed_baseline(&dev, &router, "dev");
        let rules = run_rules(&dev, &router, "dev");
        assert!(fixed.intent_correct.is_none());
        assert!(rules.intent_correct.is_some());
        assert_eq!(fixed.cases, 60);
        assert!(fixed.modeled_cost_micros > 0);
        assert!(render(&[fixed, rules]).contains("draft_unreviewed"));
    }

    struct Scripted;
    #[async_trait]
    impl Classifier for Scripted {
        async fn classify(&self, _i: ClassificationInput) -> Result<TaskClassification> {
            Err(PairError::new(ErrorCode::ProviderTimeout, "mock timeout"))
        }
    }

    #[tokio::test]
    async fn test_run_classifier_assisted_failures_counted_and_baseline_used() {
        let d = dataset();
        let router = ConfigRouter::new(test_config());
        let dev = d.split(Split::Dev);
        // Sensitive/employer cases have no provider (by design) and would add routing failures.
        let public: Vec<_> = dev
            .iter()
            .copied()
            .filter(|c| c.data_class == pair_core::types::DataClass::Public)
            .take(5)
            .collect();
        assert_eq!(public.len(), 5);
        let r = run_classifier_assisted(&public, &Scripted, &router, 42_000, "dev").await;
        assert_eq!(r.failures, 5);
        assert!(r.modeled_cost_micros > 0, "baseline still routed");
    }
}
