//! Routing pipeline: rules -> (optional) classifier with budget accounting -> router.
//! Classifier failure of any kind degrades to the baseline; it never blocks the task.
use super::baseline::classify_by_rules;
use super::config::ClassifierMode;
use super::jev::estimate_input_tokens;
use super::questions::QuestionSet;
use crate::router::{ConfigRouter, RouteConstraints, RoutePlan};
use pair_core::error::Result;
use pair_core::ids::ReservationId;
use pair_core::money::{Micros, Price};
use pair_core::traits::{BudgetEx, Classifier};
use pair_core::types::{
    ClassificationInput, DataClass, ReserveRequest, TaskClassification, TaskProfile, UsageReport,
};
use std::sync::Arc;

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
    pub fn new(
        router: ConfigRouter,
        classifier: Option<Arc<dyn Classifier>>,
        questions: QuestionSet,
    ) -> Self {
        Self {
            router,
            classifier,
            questions,
        }
    }

    pub fn router(&self) -> &ConfigRouter {
        &self.router
    }

    fn price(&self) -> Price {
        let c = &self.router.config().classifier;
        Price {
            version: c.price_version.clone(),
            input_per_mtok: Micros(c.input_price_micros_per_mtok),
            output_per_mtok: Micros::ZERO,
        }
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

    pub async fn route(
        &self,
        budget: &dyn BudgetEx,
        req: PipelineRequest,
    ) -> Result<PipelineOutcome> {
        let rules = classify_by_rules(&req.input.request);
        let profile = TaskProfile {
            intent: rules.intent.into(),
            difficulty: rules.difficulty.into(),
            data_class: req.data_class,
            needs_tools: req.needs_tools,
            est_input_tokens: req.est_input_tokens,
        };
        let mut constraints = req
            .constraints
            .clone()
            .unwrap_or_else(|| self.router.default_constraints());
        let mut classification = None;
        let mut cost = None;
        let mut note = self
            .skip_reason(&req, rules.exact_command)
            .map(str::to_string);
        if note.is_none() {
            let (c, n, spent) = self.classify_accounted(budget, &req.input).await?;
            classification = c;
            note = Some(n);
            cost = spent;
        }
        // The classifier shares the workflow budget: its spend is deducted before generation is planned.
        if let Some(spent) = cost {
            constraints.remaining_budget = constraints
                .remaining_budget
                .checked_sub(spent)
                .unwrap_or(Micros::ZERO);
        }
        let plan = self
            .router
            .plan(&profile, classification.as_ref(), &constraints)?;
        Ok(PipelineOutcome {
            plan,
            classification,
            classifier_note: note.unwrap_or_default(),
            classifier_cost: cost,
        })
    }

    async fn classify_accounted(
        &self,
        budget: &dyn BudgetEx,
        input: &ClassificationInput,
    ) -> Result<(Option<TaskClassification>, String, Option<Micros>)> {
        let Some(classifier) = &self.classifier else {
            return Ok((None, "no classifier configured".into(), None));
        };
        let price = self.price();
        let est_tokens = estimate_input_tokens(input, &self.questions);
        let Some(max_cost) = price.max_cost(est_tokens, 0) else {
            return Ok((
                None,
                "classifier skipped: cost estimate overflow".into(),
                None,
            ));
        };
        let reservation: ReservationId = match budget
            .reserve_with(ReserveRequest::classifier(
                input.task,
                max_cost,
                price.version.clone(),
            ))
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(code = ?e.code, "classifier skipped: sub-cap reservation refused");
                return Ok((None, format!("classifier skipped: {}", e.message), None));
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
                budget.reconcile(reservation, usage).await?;
                Ok((Some(c), "classifier ok".into(), Some(actual)))
            }
            Err(e) => {
                // Unknown charge: the reservation stays unresolved (spec 6: never assume zero cost).
                tracing::warn!(code = ?e.code, reservation = %reservation, "classifier failed; using baseline");
                Ok((
                    None,
                    format!("classifier failed ({:?}); baseline used", e.code),
                    Some(max_cost),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::config::test_config;
    use crate::classification::jev::{ApiKey, JevClassifier, JevSettings};
    use crate::classification::questions::test_questions;
    use crate::classification::test_db::{budget_yaml, TestDb};
    use crate::classification::test_support::{ok_body, spawn, Behavior};
    use async_trait::async_trait;
    use pair_core::error::{ErrorCode, PairError};
    use pair_core::ids::LedgerEntryId;
    use pair_core::ids::TaskId;
    use pair_core::traits::{Budget, BudgetEx};
    use pair_core::types::{BudgetCategory, LedgerEntry, ReserveRequest, TaskKind};
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeBudget {
        reserved: Mutex<Vec<ReserveRequest>>,
        reconciled: Mutex<Vec<UsageReport>>,
        refuse: bool,
    }

    #[async_trait]
    impl Budget for FakeBudget {
        async fn reserve(&self, _task: TaskId, _max_cost: Micros) -> Result<ReservationId> {
            Err(PairError::new(
                ErrorCode::Internal,
                "pipeline must reserve through BudgetEx::reserve_with",
            ))
        }
        async fn reconcile(&self, id: ReservationId, usage: UsageReport) -> Result<LedgerEntry> {
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
                    "classifier sub-cap exhausted",
                ));
            }
            self.reserved.lock().expect("lock").push(req);
            Ok(ReservationId::new())
        }

        fn task_cap(&self, _kind: TaskKind) -> Micros {
            Micros(1_000_000_000)
        }
    }

    fn pipeline(url: &str, deadline_ms: u64, mode: ClassifierMode) -> RoutingPipeline {
        let settings = JevSettings {
            endpoint: url.into(),
            model: "jev-1.13.0".into(),
            deadline: Duration::from_millis(deadline_ms),
        };
        let jev = JevClassifier::new(settings, ApiKey::new("k"), test_questions()).expect("client");
        let mut cfg = test_config();
        cfg.classifier.active_categories = vec!["coding".into()];
        cfg.task_cap_micros = 1_000_000;
        RoutingPipeline::new(
            ConfigRouter::new(cfg).with_mode(mode),
            Some(Arc::new(jev)),
            test_questions(),
        )
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
        p.route(&FakeBudget::default(), request(text))
            .await
            .expect("route")
            .plan
            .decision
            .model_id
    }

    #[tokio::test]
    async fn classifier_timeout_uses_baseline() {
        let mock = spawn(Behavior::Slow(
            Duration::from_millis(400),
            ok_body("coding", "deep", 0.99),
        ))
        .await;
        let p = pipeline(&mock.url, 50, ClassifierMode::Active);
        let out = p
            .route(&FakeBudget::default(), request("fix the failing test"))
            .await
            .expect("route");
        assert!(out.classification.is_none());
        assert!(out.classifier_note.contains("ProviderTimeout"));
        assert_eq!(
            out.plan.decision.model_id,
            baseline_model("fix the failing test").await
        );
        assert_eq!(mock.hits(), 1);
    }

    #[tokio::test]
    async fn invalid_label_uses_baseline() {
        let mut body = ok_body("coding", "deep", 0.99);
        body["answers"]["intent"]["choice"] = serde_json::json!("banana");
        let mock = spawn(Behavior::Reply(body)).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let out = p
            .route(&FakeBudget::default(), request("fix the failing test"))
            .await
            .expect("route");
        assert!(out.classification.is_none());
        assert!(out.classifier_note.contains("ClassifierInvalid"));
        assert_eq!(
            out.plan.decision.model_id,
            baseline_model("fix the failing test").await
        );
    }

    #[tokio::test]
    async fn classifier_reservation_carries_category_not_a_string_tag() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "routine", 0.95))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Shadow);
        let budget = FakeBudget::default();
        let out = p
            .route(&budget, request("fix the failing test"))
            .await
            .expect("route");
        let reserved = budget.reserved.lock().expect("lock");
        assert_eq!(reserved.len(), 1, "worst case reserved before the call");
        assert_eq!(reserved[0].category, BudgetCategory::Classifier);
        assert_eq!(reserved[0].kind, TaskKind::Default);
        assert_eq!(
            reserved[0].price_version.as_deref(),
            Some("jev-2026-10-02"),
            "plain price version, no in-band prefix"
        );
        let reconciled = budget.reconciled.lock().expect("lock");
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].price_version, "jev-2026-10-02");
        assert_eq!(reconciled[0].input_tokens, 300);
        assert_eq!(
            reconciled[0].actual_cost,
            Some(Micros(13)),
            "ceil(300 * 42000 / 1e6)"
        );
        assert_eq!(out.classifier_cost, Some(Micros(13)));
    }

    const PRICE_VERSION: &str = "jev-2026-10-02";
    const CLASSIFIER_ACTUAL_MICROS: i64 = 13;
    const ONE_CENT_MICROS: i64 = 10_000;

    async fn counted_where(db: &TestDb, filter: &str) -> i64 {
        sqlx::query_scalar(&format!(
            "SELECT COALESCE(SUM(counted_micros), 0)::BIGINT FROM budget_reservations WHERE {filter}"
        ))
        .fetch_one(&db.pool)
        .await
        .expect("sum")
    }

    fn metered(micros: i64) -> ReserveRequest {
        ReserveRequest::metered(
            TaskId::new(),
            Micros(micros),
            TaskKind::Default,
            PRICE_VERSION.to_owned(),
        )
    }

    fn classifier(micros: i64) -> ReserveRequest {
        ReserveRequest::classifier(TaskId::new(), Micros(micros), PRICE_VERSION.to_owned())
    }

    #[tokio::test]
    async fn classifier_cost_counts_toward_budget() {
        let db = TestDb::create().await;
        // month $20, day $1, classifier sub-cap one cent, task cap $0.10
        let budget = db.budget(&budget_yaml(2000, 100, 1, 10), PRICE_VERSION);
        let mock = spawn(Behavior::Reply(ok_body("coding", "routine", 0.95))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Shadow);
        let out = p
            .route(&budget, request("fix the failing test"))
            .await
            .expect("route");
        assert_eq!(out.classifier_cost, Some(Micros(CLASSIFIER_ACTUAL_MICROS)));
        assert!(out.classification.is_some());

        // Recorded under the classifier category, settled at the actual cost.
        let row: (String, String, i64) =
            sqlx::query_as("SELECT category, state, counted_micros FROM budget_reservations")
                .fetch_one(&db.pool)
                .await
                .expect("one row");
        assert_eq!(
            row,
            (
                "classifier".into(),
                "settled".into(),
                CLASSIFIER_ACTUAL_MICROS
            )
        );

        // Counts under the classifier monthly sub-cap: only the unspent remainder fits.
        let remaining = ONE_CENT_MICROS - CLASSIFIER_ACTUAL_MICROS;
        let err = budget
            .reserve_with(classifier(remaining + 1))
            .await
            .expect_err("sub-cap includes pipeline spend");
        assert_eq!(err.code, ErrorCode::BudgetExceeded);
        budget
            .reserve_with(classifier(remaining))
            .await
            .expect("exactly the remainder fits");
    }

    #[tokio::test]
    async fn classifier_cost_counts_toward_daily_cap() {
        let db = TestDb::create().await;
        // daily cap one cent; month $20; classifier sub-cap $1
        let budget = db.budget(&budget_yaml(2000, 1, 100, 10), PRICE_VERSION);
        let mock = spawn(Behavior::Reply(ok_body("coding", "routine", 0.95))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Shadow);
        p.route(&budget, request("fix the failing test"))
            .await
            .expect("route");
        let remaining = ONE_CENT_MICROS - CLASSIFIER_ACTUAL_MICROS;
        let err = budget
            .reserve_with(metered(remaining + 1))
            .await
            .expect_err("daily cap includes classifier spend");
        assert_eq!(err.code, ErrorCode::BudgetExceeded);
        assert!(err.message.contains("daily"), "{}", err.message);
        budget
            .reserve_with(metered(remaining))
            .await
            .expect("remainder fits");
    }

    #[tokio::test]
    async fn classifier_cost_counts_toward_monthly_cap() {
        let db = TestDb::create().await;
        // month cap one cent; day cap one cent (daily may not exceed monthly); sub-cap $1 is clamped by month
        let budget = db.budget(&budget_yaml(1, 1, 1, 10), PRICE_VERSION);
        let mock = spawn(Behavior::Reply(ok_body("coding", "routine", 0.95))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Shadow);
        p.route(&budget, request("fix the failing test"))
            .await
            .expect("route");
        assert_eq!(
            counted_where(&db, "category = 'classifier'").await,
            CLASSIFIER_ACTUAL_MICROS
        );
        let remaining = ONE_CENT_MICROS - CLASSIFIER_ACTUAL_MICROS;
        let err = budget
            .reserve_with(metered(remaining + 1))
            .await
            .expect_err("monthly cap includes classifier spend");
        assert_eq!(err.code, ErrorCode::BudgetExceeded);
    }

    #[tokio::test]
    async fn classifier_subcap_exceeded_falls_back_to_baseline_not_error() {
        let db = TestDb::create().await;
        let budget = db.budget(&budget_yaml(2000, 100, 1, 10), PRICE_VERSION);
        // Exhaust the one-cent classifier sub-cap.
        budget
            .reserve_with(classifier(ONE_CENT_MICROS))
            .await
            .expect("fills sub-cap");
        let mock = spawn(Behavior::Reply(ok_body("coding", "deep", 0.99))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let out = p
            .route(&budget, request("fix the failing test"))
            .await
            .expect("sub-cap refusal degrades, never errors");
        assert_eq!(mock.hits(), 0, "classifier not called without budget");
        assert!(out.classification.is_none());
        assert!(out.classifier_note.contains("skipped"));
        assert_eq!(out.classifier_cost, None);
        assert_eq!(
            out.plan.decision.model_id,
            baseline_model("fix the failing test").await
        );
        // Metered spend is unaffected by the classifier sub-cap.
        budget
            .reserve_with(metered(ONE_CENT_MICROS))
            .await
            .expect("metered category still has room");
    }

    #[tokio::test]
    async fn test_route_classifier_failure_leaves_reservation_unresolved() {
        let mock = spawn(Behavior::Status(429)).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Shadow);
        let budget = FakeBudget::default();
        p.route(&budget, request("fix the failing test"))
            .await
            .expect("route");
        assert_eq!(budget.reserved.lock().expect("lock").len(), 1);
        assert!(
            budget.reconciled.lock().expect("lock").is_empty(),
            "never assume zero cost"
        );
    }

    #[tokio::test]
    async fn test_route_subcap_exhausted_skips_classifier_and_uses_baseline() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "deep", 0.99))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let budget = FakeBudget {
            refuse: true,
            ..FakeBudget::default()
        };
        let out = p
            .route(&budget, request("fix the failing test"))
            .await
            .expect("route");
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
        let err = p
            .route(&budget, sensitive)
            .await
            .expect_err("no provider may receive sensitive data");
        assert_eq!(err.code, ErrorCode::ProviderDisallowed);
        assert_eq!(mock.hits(), 0);
        assert!(budget.reserved.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn test_route_active_valid_classification_deducts_cost_and_applies_tier() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "deep", 0.99))).await;
        let p = pipeline(&mock.url, 1000, ClassifierMode::Active);
        let out = p
            .route(&FakeBudget::default(), request("fix the failing test"))
            .await
            .expect("route");
        assert_eq!(out.plan.decision.model_id, "placeholder-deep-a");
    }
}
