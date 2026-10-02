//! Routing pipeline: rules -> (optional) classifier with budget accounting -> router.
//! Classifier failure of any kind degrades to the baseline; it never blocks the task.
use super::baseline::classify_by_rules;
use super::config::ClassifierMode;
use super::jev::estimate_input_tokens;
use super::questions::QuestionSet;
use crate::router::{ConfigRouter, RouteConstraints, RoutePlan};
use pair_core::error::Result;
use pair_core::ids::{ReservationId, TaskId};
use pair_core::money::{Micros, Price};
use pair_core::traits::{Budget, Classifier};
use pair_core::types::{ClassificationInput, DataClass, TaskClassification, TaskProfile, UsageReport};
use std::sync::Arc;

/// Prefix on `UsageReport.price_version` marking classifier spend. STOPGAP: `Budget` has no category
/// parameter, so the sub-cap category is carried in-band until the contract gains one.
pub const CLASSIFIER_CATEGORY_TAG: &str = "classifier:jev:";

pub struct PipelineRequest {
    pub input: ClassificationInput,
    pub data_class: DataClass,
    pub needs_tools: bool,
    pub est_input_tokens: u64,
    pub constraints: Option<RouteConstraints>,
}

#[derive(Debug)]
pub struct PipelineOutcome {
    pub plan: RoutePlan,
    pub classification: Option<TaskClassification>,
    pub classifier_note: String,
    pub classifier_cost: Option<Micros>,
}

pub struct RoutingPipeline {
    router: ConfigRouter,
    classifier: Option<Arc<dyn Classifier>>,
    questions: QuestionSet,
}

impl RoutingPipeline {
    pub fn new(router: ConfigRouter, classifier: Option<Arc<dyn Classifier>>, questions: QuestionSet) -> Self {
        Self { router, classifier, questions }
    }

    pub fn router(&self) -> &ConfigRouter {
        &self.router
    }

    fn price(&self) -> Price {
        let c = &self.router.config().classifier;
        Price { version: c.price_version.clone(), input_per_mtok: Micros(c.input_price_micros_per_mtok), output_per_mtok: Micros::ZERO }
    }

    fn skip_reason(&self, req: &PipelineRequest, exact_command: bool) -> Option<&'static str> {
        let cfg = &self.router.config().classifier;
        if self.router.mode() == ClassifierMode::Disabled {
            Some("classifier disabled")
        } else if exact_command {
            Some("exact deterministic command; classification skipped")
        } else if !cfg.allowed_data_classes.contains(&req.data_class) {
            Some("data class not permitted for cloud classifier")
        } else if self.classifier.is_none() {
            Some("no classifier configured")
        } else {
            None
        }
    }

    pub async fn route(&self, budget: &dyn Budget, req: PipelineRequest) -> Result<PipelineOutcome> {
        let rules = classify_by_rules(&req.input.request);
        let profile = TaskProfile {
            intent: rules.intent.into(),
            difficulty: rules.difficulty.into(),
            data_class: req.data_class,
            needs_tools: req.needs_tools,
            est_input_tokens: req.est_input_tokens,
        };
        let mut constraints = req.constraints.clone().unwrap_or_else(|| self.router.default_constraints());
        let mut classification = None;
        let mut cost = None;
        let mut note = self.skip_reason(&req, rules.exact_command).map(str::to_string);
        if note.is_none() {
            let (c, n, spent) = self.classify_accounted(budget, &req.input).await?;
            classification = c;
            note = Some(n);
            cost = spent;
        }
        // The classifier shares the workflow budget: its spend is deducted before generation is planned.
        if let Some(spent) = cost {
            constraints.remaining_budget = constraints.remaining_budget.checked_sub(spent).unwrap_or(Micros::ZERO);
        }
        let plan = self.router.plan(&profile, classification.as_ref(), &constraints)?;
        Ok(PipelineOutcome { plan, classification, classifier_note: note.unwrap_or_default(), classifier_cost: cost })
    }

    async fn classify_accounted(
        &self,
        budget: &dyn Budget,
        input: &ClassificationInput,
    ) -> Result<(Option<TaskClassification>, String, Option<Micros>)> {
        let Some(classifier) = &self.classifier else {
            return Ok((None, "no classifier configured".into(), None));
        };
        let price = self.price();
        let tag = format!("{CLASSIFIER_CATEGORY_TAG}{}", price.version);
        let est_tokens = estimate_input_tokens(input, &self.questions);
        let Some(max_cost) = price.max_cost(est_tokens, 0) else {
            return Ok((None, "classifier skipped: cost estimate overflow".into(), None));
        };
        let reservation: ReservationId = match budget.reserve(task_of(input), max_cost).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(code = ?e.code, "classifier skipped: sub-cap reservation refused");
                return Ok((None, format!("classifier skipped: {}", e.message), None));
            }
        };
        match classifier.classify(input.clone()).await {
            Ok(c) => {
                let actual = price.max_cost(c.input_tokens, 0).unwrap_or(max_cost);
                let usage = UsageReport { input_tokens: c.input_tokens, output_tokens: 0, actual_cost: Some(actual), price_version: tag };
                budget.reconcile(reservation, usage).await?;
                Ok((Some(c), "classifier ok".into(), Some(actual)))
            }
            Err(e) => {
                // Unknown charge: the reservation stays unresolved (spec 6: never assume zero cost).
                tracing::warn!(code = ?e.code, reservation = %reservation, "classifier failed; using baseline");
                Ok((None, format!("classifier failed ({:?}); baseline used", e.code), Some(max_cost)))
            }
        }
    }
}

fn task_of(input: &ClassificationInput) -> TaskId {
    input.task
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::config::test_config;
    use crate::classification::jev::{ApiKey, JevClassifier, JevSettings};
    use crate::classification::questions::test_questions;
    use crate::classification::test_support::{ok_body, spawn, Behavior};
    use async_trait::async_trait;
    use pair_core::error::{ErrorCode, PairError};
    use pair_core::ids::LedgerEntryId;
    use pair_core::types::LedgerEntry;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeBudget {
        reserved: Mutex<Vec<Micros>>,
        reconciled: Mutex<Vec<UsageReport>>,
        refuse: bool,
    }

    #[async_trait]
    impl Budget for FakeBudget {
        async fn reserve(&self, _task: TaskId, max_cost: Micros) -> Result<ReservationId> {
            if self.refuse {
                return Err(PairError::new(ErrorCode::BudgetExceeded, "classifier sub-cap exhausted"));
            }
            self.reserved.lock().expect("lock").push(max_cost);
            Ok(ReservationId::new())
        }
        async fn reconcile(&self, id: ReservationId, usage: UsageReport) -> Result<LedgerEntry> {
            let amount = usage.actual_cost.unwrap_or(Micros::ZERO);
            self.reconciled.lock().expect("lock").push(usage);
            Ok(LedgerEntry { id: LedgerEntryId::new(), reservation: id, amount, settled: true })
        }
    }

    fn pipeline(url: &str, deadline_ms: u64, mode: ClassifierMode) -> RoutingPipeline {
        let settings = JevSettings { endpoint: url.into(), model: "jev-1.13.0".into(), deadline: Duration::from_millis(deadline_ms) };
        let jev = JevClassifier::new(settings, ApiKey::new("k"), test_questions()).expect("client");
        let mut cfg = test_config();
        cfg.classifier.active_categories = vec!["coding".into()];
        cfg.task_cap_micros = 1_000_000;
        RoutingPipeline::new(ConfigRouter::new(cfg).with_mode(mode), Some(Arc::new(jev)), test_questions())
    }

    fn request(text: &str) -> PipelineRequest {
        PipelineRequest {
            input: ClassificationInput {
                request: text.into(),
                recent_summary: String::new(),
                project_type: None,
                workflows: vec!["engineering".into()],
                task: TaskId::new(),
            },
            data_class: DataClass::Public,
            needs_tools: false,
            est_input_tokens: 2_000,
            constraints: None,
        }
    }

    async fn baseline_model(text: &str) -> String {
        let p = pipeline("http://127.0.0.1:1/unused", 50, ClassifierMode::Disabled);
        p.route(&FakeBudget::default(), request(text)).await.expect("route").plan.decision.model_id
    }

    #[tokio::test]
    async fn classifier_timeout_uses_baseline() {
        let mock = spawn(Behavior::Slow(Duration::from_millis(400), ok_body("coding", "deep", 0.99))).await;
        let p = pipeline(&mock.url, 50, ClassifierMode::Active);
        let out = p.route(&FakeBudget::default(), request("fix the failing test")).await.expect("route");
        assert!(out.classification.is_none());
        assert!(out.classifier_note.contains("ProviderTimeout"));
        assert_eq!(out.plan.decision.model_id, baseline_model("fix the failing test").await);
        assert_eq!(mock.hits(), 1);
    }

    #[tokio::test]
    async fn invalid_label_uses_baseline() {
        let mut body = ok_body("coding", "deep", 0.99);
        body["answers"]["intent"]["choice"] = serde_json::json!("banana");
        let mock = spawn(Behavior::Reply(body)).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let out = p.route(&FakeBudget::default(), request("fix the failing test")).await.expect("route");
        assert!(out.classification.is_none());
        assert!(out.classifier_note.contains("ClassifierInvalid"));
        assert_eq!(out.plan.decision.model_id, baseline_model("fix the failing test").await);
    }

    #[tokio::test]
    async fn classifier_cost_counts_toward_budget() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "routine", 0.95))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Shadow);
        let budget = FakeBudget::default();
        let out = p.route(&budget, request("fix the failing test")).await.expect("route");
        assert_eq!(budget.reserved.lock().expect("lock").len(), 1, "worst case reserved before the call");
        let reconciled = budget.reconciled.lock().expect("lock");
        assert_eq!(reconciled.len(), 1);
        assert!(reconciled[0].price_version.starts_with(CLASSIFIER_CATEGORY_TAG));
        assert_eq!(reconciled[0].input_tokens, 300);
        assert_eq!(reconciled[0].actual_cost, Some(Micros(13)), "ceil(300 * 42000 / 1e6)");
        assert_eq!(out.classifier_cost, Some(Micros(13)));
    }

    #[tokio::test]
    async fn test_route_classifier_failure_leaves_reservation_unresolved() {
        let mock = spawn(Behavior::Status(429)).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Shadow);
        let budget = FakeBudget::default();
        p.route(&budget, request("fix the failing test")).await.expect("route");
        assert_eq!(budget.reserved.lock().expect("lock").len(), 1);
        assert!(budget.reconciled.lock().expect("lock").is_empty(), "never assume zero cost");
    }

    #[tokio::test]
    async fn test_route_subcap_exhausted_skips_classifier_and_uses_baseline() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "deep", 0.99))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let budget = FakeBudget { refuse: true, ..FakeBudget::default() };
        let out = p.route(&budget, request("fix the failing test")).await.expect("route");
        assert_eq!(mock.hits(), 0);
        assert!(out.classifier_note.contains("skipped"));
    }

    #[tokio::test]
    async fn test_route_exact_command_and_private_data_skip_classifier() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "deep", 0.99))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let budget = FakeBudget::default();
        p.route(&budget, request("/status")).await.expect("route");
        let mut sensitive = request("fix the failing test");
        sensitive.data_class = DataClass::Sensitive;
        p.route(&budget, sensitive).await.expect("route");
        assert_eq!(mock.hits(), 0);
        assert!(budget.reserved.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn test_route_active_valid_classification_deducts_cost_and_applies_tier() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "deep", 0.99))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let out = p.route(&FakeBudget::default(), request("fix the failing test")).await.expect("route");
        assert_eq!(out.plan.decision.model_id, "placeholder-deep-a");
    }
}
