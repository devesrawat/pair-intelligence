//! Evaluation harness for the three routing strategies of spec 6.1.
//! Everything computed here is MODELED or measured on DRAFT labels. Nothing in this module is evidence of
//! routing quality: downstream task acceptance requires real workflow runs and owner-reviewed labels.
use super::baseline::classify_by_rules;
use super::config::{ClassifierMode, RoutingConfig};
use super::jev::estimate_input_tokens;
use super::questions::QuestionSet;
use crate::router::ConfigRouter;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::{Micros, Price};
use pair_core::traits::{BudgetEx, Classifier};
use pair_core::types::{
    ClassificationInput, DataClass, ReserveRequest, TaskClassification, TaskProfile, UsageReport,
};
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
///
/// Every classifier call is reserved through `budget` first and reconciled with the observed usage
/// afterwards, exactly like the production pipeline (spec 6: no unmetered spend). A refused reservation skips
/// the call (counted as a failure); a failed call leaves its reservation unresolved at the reserved amount.
pub async fn run_classifier_assisted(
    cases: &[&EvalCase],
    classifier: &dyn Classifier,
    router: &ConfigRouter,
    budget: &dyn BudgetEx,
    questions: &QuestionSet,
    split: &str,
) -> StrategyReport {
    let cfg = &router.config().classifier;
    let price = Price {
        version: cfg.price_version.clone(),
        input_per_mtok: Micros(cfg.input_price_micros_per_mtok),
        output_per_mtok: Micros::ZERO,
    };
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
        let (c, charged) = classify_reserved(classifier, budget, &price, questions, &input).await;
        lat.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
        r.classifier_cost_micros = r.classifier_cost_micros.saturating_add(charged.0);
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

/// Reserve, call, reconcile. Returns the classification (if any) and the amount charged to the budget.
async fn classify_reserved(
    classifier: &dyn Classifier,
    budget: &dyn BudgetEx,
    price: &Price,
    questions: &QuestionSet,
    input: &ClassificationInput,
) -> (Option<TaskClassification>, Micros) {
    let est_tokens = estimate_input_tokens(input, questions);
    let Some(max_cost) = price.max_cost(est_tokens, 0) else {
        tracing::warn!("classifier eval call skipped: cost estimate overflow");
        return (None, Micros::ZERO);
    };
    let reservation = match budget
        .reserve_with(ReserveRequest::classifier(
            input.task,
            max_cost,
            price.version.clone(),
        ))
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(code = ?e.code, "classifier eval call skipped: reservation refused");
            return (None, Micros::ZERO);
        }
    };
    match classifier.classify(input.clone()).await {
        Ok(c) => {
            let actual = price.max_cost(c.input_tokens, 0).unwrap_or(max_cost);
            let usage = UsageReport {
                input_tokens: c.input_tokens,
                output_tokens: 0,
                actual_cost: Some(actual),
                price_version: price.version.clone(),
            };
            if let Err(e) = budget.reconcile(reservation, usage).await {
                tracing::error!(code = ?e.code, %reservation, "classifier eval reconcile failed; marking the reservation unresolved");
                leave_unresolved(budget, reservation, price).await;
                return (Some(c), max_cost);
            }
            (Some(c), actual)
        }
        Err(e) => {
            // Unknown charge: record it as unresolved so the full reservation stays counted AND
            // visible to the unresolved-reservation alert (spec 6: never assume zero cost).
            tracing::warn!(code = ?e.code, %reservation, "classifier eval call failed; reservation left unresolved");
            leave_unresolved(budget, reservation, price).await;
            (None, max_cost)
        }
    }
}

/// Reconcile with an unknown actual cost (`actual_cost: None`), exactly like `pipeline.rs`: the
/// reservation becomes `unresolved` instead of staying `held` forever.
async fn leave_unresolved(budget: &dyn BudgetEx, reservation: ReservationId, price: &Price) {
    let unknown = UsageReport {
        input_tokens: 0,
        output_tokens: 0,
        actual_cost: None,
        price_version: price.version.clone(),
    };
    if let Err(e) = budget.reconcile(reservation, unknown).await {
        tracing::error!(code = ?e.code, %reservation, "could not mark the classifier reservation unresolved");
    }
}

/// Refuses to evaluate against a classifier endpoint that can spend real money when no budget is
/// available to meter it. Loopback endpoints (local mocks) need no budget.
pub fn require_budget_for_real_endpoint(endpoint: &str, budget_available: bool) -> Result<()> {
    if budget_available || endpoint_is_loopback(endpoint) {
        return Ok(());
    }
    Err(PairError::new(
        ErrorCode::BudgetExceeded,
        format!(
            "refusing to call classifier endpoint {endpoint}: it is not loopback and no budget was supplied \
             (set DATABASE_URL so classifier calls are reserved and reconciled)"
        ),
    ))
}

fn endpoint_is_loopback(endpoint: &str) -> bool {
    use url::Host;
    match url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host().map(|h| h.to_owned()))
    {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
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
    use crate::classification::questions::test_questions;
    use crate::classification::questions::{DIFFICULTY_LABELS, INTENT_LABELS};
    use async_trait::async_trait;
    use pair_budget::{BudgetConfig, PgBudget, PriceBook};
    use pair_core::ids::{LedgerEntryId, ReservationId};
    use pair_core::traits::Budget;
    use pair_core::types::{BudgetCategory, LedgerEntry, TaskKind};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{ConnectOptions, PgPool};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

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

    struct OkClassifier;
    #[async_trait]
    impl Classifier for OkClassifier {
        async fn classify(&self, _i: ClassificationInput) -> Result<TaskClassification> {
            Ok(TaskClassification {
                model_version: "jev-test".into(),
                question_version: "q1".into(),
                intent: "coding".into(),
                difficulty: "substantial".into(),
                intent_probabilities: Default::default(),
                difficulty_probabilities: Default::default(),
                intent_confidence: 0.9,
                difficulty_confidence: 0.9,
                input_tokens: 1_000,
                latency_ms: 1,
                request_id: "r".into(),
            })
        }
    }

    #[derive(Default)]
    struct FakeBudget {
        reserved: Mutex<Vec<ReserveRequest>>,
        reconciled: Mutex<Vec<UsageReport>>,
        refuse: bool,
        /// Fail the next `reconcile` call (it records nothing), then behave normally.
        fail_next_reconcile: AtomicBool,
    }

    #[async_trait]
    impl Budget for FakeBudget {
        async fn reserve(&self, _t: TaskId, _m: Micros) -> Result<ReservationId> {
            Err(PairError::new(ErrorCode::Internal, "must use reserve_with"))
        }
        async fn reconcile(&self, id: ReservationId, usage: UsageReport) -> Result<LedgerEntry> {
            if self.fail_next_reconcile.swap(false, Ordering::SeqCst) {
                return Err(PairError::new(ErrorCode::Internal, "reconcile failed"));
            }
            let amount = usage.actual_cost.unwrap_or(Micros::ZERO);
            self.reconciled.lock().expect("lock").push(usage);
            Ok(LedgerEntry {
                id: LedgerEntryId::new(),
                reservation: id,
                amount,
                settled: true,
            })
        }
    }

    #[async_trait]
    impl BudgetEx for FakeBudget {
        async fn reserve_with(&self, req: ReserveRequest) -> Result<ReservationId> {
            if self.refuse {
                return Err(PairError::new(
                    ErrorCode::BudgetExceeded,
                    "sub-cap exhausted",
                ));
            }
            self.reserved.lock().expect("lock").push(req);
            Ok(ReservationId::new())
        }
        fn task_cap(&self, _kind: TaskKind) -> Micros {
            Micros(1_000_000_000)
        }
    }

    fn public_cases(d: &Dataset, n: usize) -> Vec<&EvalCase> {
        d.split(Split::Dev)
            .into_iter()
            .filter(|c| c.data_class == pair_core::types::DataClass::Public)
            .take(n)
            .collect()
    }

    #[tokio::test]
    async fn test_run_classifier_assisted_failures_counted_and_baseline_used() {
        let d = dataset();
        let router = ConfigRouter::new(test_config());
        // Sensitive/employer cases have no provider (by design) and would add routing failures.
        let public = public_cases(&d, 5);
        assert_eq!(public.len(), 5);
        let budget = FakeBudget::default();
        let r = run_classifier_assisted(
            &public,
            &Scripted,
            &router,
            &budget,
            &test_questions(),
            "dev",
        )
        .await;
        assert_eq!(r.failures, 5);
        assert!(r.modeled_cost_micros > 0, "baseline still routed");
        assert!(
            r.classifier_cost_micros > 0,
            "a failed call is charged at the reserved amount"
        );
        let reconciled = budget.reconciled.lock().expect("lock");
        assert_eq!(reconciled.len(), 5, "every failed call is recorded");
        assert!(
            reconciled.iter().all(|u| u.actual_cost.is_none()),
            "failed calls are reconciled as unresolved, never at zero"
        );
    }

    #[tokio::test]
    async fn classifier_eval_reconcile_failure_falls_back_to_unresolved() {
        let d = dataset();
        let router = ConfigRouter::new(test_config());
        let cases = public_cases(&d, 1);
        let budget = FakeBudget::default();
        budget.fail_next_reconcile.store(true, Ordering::SeqCst);
        let r = run_classifier_assisted(
            &cases,
            &OkClassifier,
            &router,
            &budget,
            &test_questions(),
            "dev",
        )
        .await;
        let reconciled = budget.reconciled.lock().expect("lock");
        assert_eq!(reconciled.len(), 1, "a second, unresolved report was made");
        assert_eq!(reconciled[0].actual_cost, None, "never assume the charge");
        assert!(
            r.classifier_cost_micros > 0,
            "charged at the reserved amount"
        );
    }

    const BUDGET_YAML: &str = "budget:\n  currency: USD\n  metered_monthly_cap: 20.00\n  \
        metered_daily_cap: 5.00\n  classifier_monthly_subcap: 1.00\n  default_task_cap: 0.10\n  \
        research_task_cap: 0.10\n  coding_task_cap: 1.00\n  auto_top_up: false\n\
        schedule:\n  timezone: Asia/Kolkata\n";

    /// One uniquely named, fully migrated scratch database per test; dropped afterwards.
    struct ScratchDb {
        pool: PgPool,
        name: String,
        admin_url: String,
    }

    impl ScratchDb {
        async fn create() -> Self {
            let admin_url = std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://pair:pair@127.0.0.1:55432/pair".to_owned());
            let name = format!("pair_t_models_{}", uuid::Uuid::new_v4().simple());
            let mut admin = PgConnectOptions::from_str(&admin_url)
                .expect("url")
                .connect()
                .await
                .expect("admin connect");
            sqlx::query(&format!("CREATE DATABASE {name}"))
                .execute(&mut admin)
                .await
                .expect("create scratch db");
            let opts = PgConnectOptions::from_str(&admin_url)
                .expect("url")
                .database(&name);
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .connect_with(opts)
                .await
                .expect("scratch connect");
            pair_budget::MIGRATOR.run(&pool).await.expect("migrate");
            Self {
                pool,
                name,
                admin_url,
            }
        }
    }

    impl Drop for ScratchDb {
        fn drop(&mut self) {
            let (url, name) = (self.admin_url.clone(), self.name.clone());
            let cleanup = std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async {
                    let Ok(opts) = PgConnectOptions::from_str(&url) else {
                        return;
                    };
                    if let Ok(mut admin) = opts.connect().await {
                        let _ =
                            sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                                .execute(&mut admin)
                                .await;
                    }
                });
            });
            let _ = cleanup.join();
        }
    }

    #[tokio::test]
    async fn classifier_eval_failed_call_leaves_reservation_unresolved() {
        let db = ScratchDb::create().await;
        let d = dataset();
        let router = ConfigRouter::new(test_config());
        let cases = public_cases(&d, 3);
        let version = router.config().classifier.price_version.clone();
        let budget = PgBudget::new(
            db.pool.clone(),
            BudgetConfig::from_yaml(BUDGET_YAML).expect("budget config"),
            PriceBook::new(Some(version), []),
        );

        let r = run_classifier_assisted(
            &cases,
            &Scripted,
            &router,
            &budget,
            &test_questions(),
            "dev",
        )
        .await;

        assert_eq!(r.failures, 3);
        let states: Vec<(String, i64)> = sqlx::query_as(
            "SELECT state, count(*) FROM budget_reservations GROUP BY state ORDER BY state",
        )
        .fetch_all(&db.pool)
        .await
        .expect("states");
        assert_eq!(
            states,
            vec![("unresolved".to_owned(), 3)],
            "a failed classifier call must surface as unresolved, not sit `held` forever"
        );
    }

    #[tokio::test]
    async fn classifier_eval_reserves_per_call() {
        let d = dataset();
        let router = ConfigRouter::new(test_config());
        let cases = public_cases(&d, 4);
        let budget = FakeBudget::default();
        let r = run_classifier_assisted(
            &cases,
            &OkClassifier,
            &router,
            &budget,
            &test_questions(),
            "dev",
        )
        .await;
        let reserved = budget.reserved.lock().expect("lock");
        assert_eq!(reserved.len(), 4, "one reservation per classifier call");
        assert!(reserved
            .iter()
            .all(|q| q.category == BudgetCategory::Classifier && q.max_cost.0 > 0));
        let tasks: std::collections::HashSet<_> = reserved.iter().map(|q| q.task).collect();
        assert_eq!(tasks.len(), 4, "each call is its own task");
        let reconciled = budget.reconciled.lock().expect("lock");
        assert_eq!(reconciled.len(), 4);
        assert!(reconciled.iter().all(|u| u.actual_cost.is_some()));
        assert!(r.classifier_cost_micros > 0);
        assert_eq!(r.failures, 0);
    }

    #[tokio::test]
    async fn classifier_eval_refused_reservation_skips_the_call() {
        let d = dataset();
        let router = ConfigRouter::new(test_config());
        let cases = public_cases(&d, 3);
        let budget = FakeBudget {
            refuse: true,
            ..Default::default()
        };
        let r = run_classifier_assisted(
            &cases,
            &OkClassifier,
            &router,
            &budget,
            &test_questions(),
            "dev",
        )
        .await;
        assert_eq!(r.failures, 3);
        assert_eq!(r.classifier_cost_micros, 0);
        assert!(budget.reconciled.lock().expect("lock").is_empty());
    }

    #[test]
    fn classifier_eval_refuses_real_endpoint_without_budget() {
        let real = "https://api.typesafe.ai/v1/systemone";
        let err = require_budget_for_real_endpoint(real, false).expect_err("refused");
        assert_eq!(err.code, ErrorCode::BudgetExceeded);
        assert!(require_budget_for_real_endpoint(real, true).is_ok());
        for local in [
            "http://127.0.0.1:9/x",
            "http://localhost:9",
            "http://[::1]:9/x",
        ] {
            assert!(
                require_budget_for_real_endpoint(local, false).is_ok(),
                "{local}"
            );
        }
        assert!(require_budget_for_real_endpoint("http://127.0.0.1.evil.com/x", false).is_err());
        assert!(require_budget_for_real_endpoint("not a url", false).is_err());
    }
}
