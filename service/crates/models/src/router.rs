//! Router: hard constraints first, then baseline tier, then (active mode only) classifier-recommended tier.
//! Classifier output is advisory: it can pick among already-eligible models and never widens the set.
use crate::classification::config::{Candidate, ClassifierMode, RoutingConfig};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::money::{Micros, Price};
use pair_core::traits::Router;
use pair_core::types::{RouteDecision, TaskClassification, TaskProfile};
use std::collections::HashSet;

/// Hard-constraint stages in the order spec 6 applies them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Provider,
    Capability,
    Context,
    Availability,
    Budget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exclusion {
    pub model_id: String,
    pub stage: Stage,
    pub detail: String,
}

/// Per-call constraints that the sync `Router::select` signature cannot carry.
#[derive(Debug, Clone)]
pub struct RouteConstraints {
    pub extra_capabilities: Vec<String>,
    pub unavailable: HashSet<String>,
    pub remaining_budget: Micros,
}

/// What the classifier recommended and whether PAIR would act on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    pub tier: Option<String>,
    pub applied: bool,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub decision: RouteDecision,
    /// Ordered fallbacks; `attempt_order` caps primary + fallbacks at `max_model_attempts`.
    pub fallbacks: Vec<String>,
    pub exclusions: Vec<Exclusion>,
    pub recommendation: Option<Recommendation>,
    max_attempts: usize,
}

impl RoutePlan {
    pub fn attempt_order(&self) -> Vec<String> {
        std::iter::once(self.decision.model_id.clone())
            .chain(self.fallbacks.iter().cloned())
            .take(self.max_attempts)
            .collect()
    }
}

/// Counts model attempts (generation, repair, escalation) against the configured maximum.
#[derive(Debug, Clone, Copy)]
pub struct AttemptLimiter {
    max: usize,
    used: usize,
}

impl AttemptLimiter {
    pub fn new(max: usize) -> Self {
        Self { max, used: 0 }
    }
    /// Returns the limiter after recording an attempt, or `BudgetExceeded` once the maximum is used.
    pub fn begin_attempt(self) -> Result<Self> {
        if self.used >= self.max {
            return Err(PairError::new(
                ErrorCode::BudgetExceeded,
                format!("maximum of {} model attempts reached", self.max),
            ));
        }
        Ok(Self {
            used: self.used + 1,
            ..self
        })
    }
}

pub struct ConfigRouter {
    cfg: RoutingConfig,
    mode: ClassifierMode,
}

impl ConfigRouter {
    pub fn new(cfg: RoutingConfig) -> Self {
        let mode = cfg.classifier.mode;
        Self { cfg, mode }
    }

    pub fn with_mode(mut self, mode: ClassifierMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn mode(&self) -> ClassifierMode {
        self.mode
    }

    pub fn config(&self) -> &RoutingConfig {
        &self.cfg
    }

    pub fn default_constraints(&self) -> RouteConstraints {
        RouteConstraints {
            extra_capabilities: Vec::new(),
            unavailable: HashSet::new(),
            remaining_budget: Micros(self.cfg.task_cap_micros),
        }
    }

    fn check(
        &self,
        c: &Candidate,
        profile: &TaskProfile,
        k: &RouteConstraints,
    ) -> Option<Exclusion> {
        let excl = |stage, detail: String| {
            Some(Exclusion {
                model_id: c.id.clone(),
                stage,
                detail,
            })
        };
        let allowed = self.cfg.data_class_providers.get(&profile.data_class);
        if !allowed.is_some_and(|p| p.contains(&c.provider)) {
            return excl(
                Stage::Provider,
                format!(
                    "provider {} not allowed for {:?} data",
                    c.provider, profile.data_class
                ),
            );
        }
        let needs_tools = profile.needs_tools.then_some("tools");
        let missing = needs_tools
            .into_iter()
            .chain(k.extra_capabilities.iter().map(String::as_str))
            .find(|cap| !c.capabilities.iter().any(|have| have == cap));
        if let Some(cap) = missing {
            return excl(Stage::Capability, format!("missing capability {cap}"));
        }
        let needed = profile
            .est_input_tokens
            .saturating_add(self.cfg.max_output_tokens);
        if needed > c.context_tokens {
            return excl(
                Stage::Context,
                format!("needs {needed} tokens, window is {}", c.context_tokens),
            );
        }
        if !c.available || k.unavailable.contains(&c.id) {
            return excl(Stage::Availability, "provider unavailable".into());
        }
        let price = Price {
            version: "routing".into(),
            input_per_mtok: Micros(c.input_price_micros_per_mtok),
            output_per_mtok: Micros(c.output_price_micros_per_mtok),
        };
        match price.max_cost(profile.est_input_tokens, self.cfg.max_output_tokens) {
            Some(cost) if cost <= k.remaining_budget => None,
            Some(cost) => excl(
                Stage::Budget,
                format!(
                    "worst case {} micros exceeds remaining {}",
                    cost.0, k.remaining_budget.0
                ),
            ),
            None => excl(Stage::Budget, "cost overflow".into()),
        }
    }

    /// Judge the classifier output. `Ok(tier)` means a tier recommendation that passed validation
    /// and the calibrated threshold; `Err(reason)` means use the baseline.
    fn recommend(&self, c: &TaskClassification) -> std::result::Result<String, String> {
        let labels_ok = |l: &str, ps: &std::collections::BTreeMap<String, f64>| ps.contains_key(l);
        if !labels_ok(&c.intent, &c.intent_probabilities)
            || !labels_ok(&c.difficulty, &c.difficulty_probabilities)
        {
            return Err("invalid label".into());
        }
        if c.intent == "mixed" || c.intent == "uncertain" || c.difficulty == "uncertain" {
            return Err(format!("{}/{} defers to baseline", c.intent, c.difficulty));
        }
        let Some(t) = self
            .cfg
            .thresholds_for(&c.model_version, &c.question_version)
        else {
            return Err(format!(
                "no calibrated threshold for {} / {}",
                c.model_version, c.question_version
            ));
        };
        if c.intent_confidence < t.intent || c.difficulty_confidence < t.difficulty {
            return Err("below calibrated threshold".into());
        }
        self.cfg
            .tier_by_difficulty
            .get(&c.difficulty)
            .cloned()
            .ok_or_else(|| format!("no tier for difficulty {}", c.difficulty))
    }

    fn tier_preference(&self, desired: &str, baseline: &str) -> Vec<String> {
        let order = &self.cfg.tier_order;
        let base_idx = order.iter().position(|t| t == baseline).unwrap_or(0);
        let mut prefs = vec![desired.to_string(), baseline.to_string()];
        prefs.extend(order.iter().skip(base_idx + 1).cloned());
        prefs.extend(order.iter().take(base_idx).rev().cloned());
        let mut seen = HashSet::new();
        prefs.retain(|t| seen.insert(t.clone()));
        prefs
    }

    fn no_eligible_error(&self, exclusions: &[Exclusion]) -> PairError {
        let deepest = exclusions.iter().map(|e| e.stage).max();
        let code = match deepest {
            Some(Stage::Provider) => ErrorCode::ProviderDisallowed,
            Some(Stage::Capability) => ErrorCode::CapabilityMismatch,
            None => ErrorCode::InvalidInput,
            Some(Stage::Context) => ErrorCode::ContextOverflow,
            Some(Stage::Availability) => ErrorCode::ProviderUnavailable,
            Some(Stage::Budget) => ErrorCode::BudgetExceeded,
        };
        let detail: Vec<String> = exclusions
            .iter()
            .map(|e| format!("{} ({:?}: {})", e.model_id, e.stage, e.detail))
            .collect();
        PairError::new(code, format!("no eligible model: {}", detail.join("; ")))
    }

    /// Fixed baseline for the profile, ignoring any classifier (evaluation strategies 1 and 2).
    pub fn select_baseline(&self, profile: &TaskProfile) -> Result<RouteDecision> {
        self.plan_as(
            ClassifierMode::Disabled,
            false,
            profile,
            None,
            &self.default_constraints(),
        )
        .map(|p| p.decision)
    }

    /// Evaluation only: what active mode would pick if every category were approved.
    pub fn select_hypothetical_active(
        &self,
        profile: &TaskProfile,
        classification: Option<&TaskClassification>,
    ) -> Result<RouteDecision> {
        self.plan_as(
            ClassifierMode::Active,
            true,
            profile,
            classification,
            &self.default_constraints(),
        )
        .map(|p| p.decision)
    }

    pub fn plan(
        &self,
        profile: &TaskProfile,
        classification: Option<&TaskClassification>,
        constraints: &RouteConstraints,
    ) -> Result<RoutePlan> {
        self.plan_as(self.mode, false, profile, classification, constraints)
    }

    fn plan_as(
        &self,
        mode: ClassifierMode,
        approve_all: bool,
        profile: &TaskProfile,
        classification: Option<&TaskClassification>,
        constraints: &RouteConstraints,
    ) -> Result<RoutePlan> {
        let (eligible, exclusions): (Vec<&Candidate>, Vec<Exclusion>) = {
            let mut ok = Vec::new();
            let mut bad = Vec::new();
            for c in &self.cfg.candidates {
                match self.check(c, profile, constraints) {
                    None => ok.push(c),
                    Some(e) => bad.push(e),
                }
            }
            (ok, bad)
        };
        if eligible.is_empty() {
            return Err(self.no_eligible_error(&exclusions));
        }
        let baseline = self.cfg.baseline_tier(&profile.intent).to_string();
        let verdict = match (mode, classification) {
            (ClassifierMode::Disabled, _) | (_, None) => None,
            (_, Some(c)) => Some((c, self.recommend(c))),
        };
        let approved = |c: &TaskClassification| {
            approve_all || self.cfg.classifier.active_categories.contains(&c.intent)
        };
        let (desired, recommendation) = match &verdict {
            None => (baseline.clone(), None),
            Some((_, Err(why))) => (
                baseline.clone(),
                Some(Recommendation {
                    tier: None,
                    applied: false,
                    note: format!("baseline: {why}"),
                }),
            ),
            Some((c, Ok(tier))) if mode == ClassifierMode::Active && approved(c) => (
                tier.clone(),
                Some(Recommendation {
                    tier: Some(tier.clone()),
                    applied: true,
                    note: "active: category approved".into(),
                }),
            ),
            Some((_, Ok(tier))) => {
                let note = if mode == ClassifierMode::Shadow {
                    "shadow: recommendation recorded only"
                } else {
                    "active: category not approved"
                };
                (
                    baseline.clone(),
                    Some(Recommendation {
                        tier: Some(tier.clone()),
                        applied: false,
                        note: note.into(),
                    }),
                )
            }
        };
        let mut ordered: Vec<&Candidate> = Vec::new();
        for tier in self.tier_preference(&desired, &baseline) {
            ordered.extend(eligible.iter().copied().filter(|c| c.tier == tier));
        }
        let Some(primary) = ordered.first() else {
            return Err(self.no_eligible_error(&exclusions));
        };
        let reason = match &recommendation {
            Some(r) => format!("tier={desired}; baseline={baseline}; {}", r.note),
            None => format!("tier={desired}; baseline={baseline}; no classifier recommendation"),
        };
        let fallbacks = ordered.iter().skip(1).map(|c| c.id.clone()).collect();
        Ok(RoutePlan {
            decision: RouteDecision {
                model_id: primary.id.clone(),
                reason,
                classifier_mode: mode.as_str().into(),
            },
            fallbacks,
            exclusions,
            recommendation,
            max_attempts: self.cfg.max_model_attempts,
        })
    }
}

impl Router for ConfigRouter {
    fn select(
        &self,
        profile: &TaskProfile,
        classification: Option<&TaskClassification>,
    ) -> Result<RouteDecision> {
        self.plan(profile, classification, &self.default_constraints())
            .map(|p| p.decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::config::test_config;
    use pair_core::types::DataClass;
    use std::collections::BTreeMap;

    fn profile(intent: &str, data_class: DataClass) -> TaskProfile {
        TaskProfile {
            intent: intent.into(),
            difficulty: "substantial".into(),
            data_class,
            needs_tools: false,
            est_input_tokens: 2_000,
        }
    }

    fn classification(intent: &str, difficulty: &str, conf: f64) -> TaskClassification {
        let probs = |label: &str| BTreeMap::from([(label.to_string(), conf)]);
        TaskClassification {
            model_version: "jev-1.13.0".into(),
            question_version: "2026-10-03.1".into(),
            intent: intent.into(),
            difficulty: difficulty.into(),
            intent_probabilities: probs(intent),
            difficulty_probabilities: probs(difficulty),
            intent_confidence: conf,
            difficulty_confidence: conf,
            input_tokens: 300,
            latency_ms: 80,
            request_id: "r".into(),
        }
    }

    const TEST_CAP_MICROS: i64 = 1_000_000;

    fn router(mode: ClassifierMode) -> ConfigRouter {
        let mut cfg = test_config();
        cfg.classifier.active_categories = vec!["coding".into()];
        cfg.task_cap_micros = TEST_CAP_MICROS;
        ConfigRouter::new(cfg).with_mode(mode)
    }

    #[test]
    fn capability_mismatch_excluded() {
        let r = router(ClassifierMode::Disabled);
        let mut p = profile("coding", DataClass::Public);
        p.needs_tools = true;
        let plan = r.plan(&p, None, &r.default_constraints()).expect("plan");
        assert!(
            plan.attempt_order().iter().all(|id| id != "gemma4:31b"),
            "text-only model lacks tools"
        );
        assert!(plan
            .exclusions
            .iter()
            .any(|e| e.stage == Stage::Capability && e.model_id == "gemma4:31b"));
        let mut k = r.default_constraints();
        k.extra_capabilities = vec!["vision".into()];
        let err = r.plan(&p, None, &k).expect_err("nobody has vision");
        assert_eq!(err.code, ErrorCode::CapabilityMismatch);
        assert!(err.message.contains("vision"));
    }

    #[test]
    fn private_data_excludes_disallowed_provider() {
        let r = router(ClassifierMode::Disabled);
        let plan = r
            .plan(
                &profile("coding", DataClass::Sensitive),
                None,
                &r.default_constraints(),
            )
            .expect("plan");
        let cfg = r.config();
        for id in plan.attempt_order() {
            let c = cfg
                .candidates
                .iter()
                .find(|c| c.id == id)
                .expect("candidate");
            assert_eq!(c.provider, "anthropic");
        }
        let mut locked = test_config();
        locked
            .data_class_providers
            .insert(DataClass::Employer, vec![]);
        let err = ConfigRouter::new(locked)
            .select(&profile("coding", DataClass::Employer), None)
            .expect_err("none allowed");
        assert_eq!(err.code, ErrorCode::ProviderDisallowed);
    }

    #[test]
    fn unavailable_provider_skipped() {
        let r = router(ClassifierMode::Disabled);
        let p = profile("coding", DataClass::Public);
        let first = r
            .plan(&p, None, &r.default_constraints())
            .expect("plan")
            .decision
            .model_id;
        let mut k = r.default_constraints();
        k.unavailable.insert(first.clone());
        let next = r.plan(&p, None, &k).expect("plan");
        assert_ne!(next.decision.model_id, first);
        assert!(next
            .exclusions
            .iter()
            .any(|e| e.stage == Stage::Availability && e.model_id == first));
        k.unavailable
            .extend(r.config().candidates.iter().map(|c| c.id.clone()));
        assert_eq!(
            r.plan(&p, None, &k).expect_err("all down").code,
            ErrorCode::ProviderUnavailable
        );
    }

    #[test]
    fn max_three_attempts() {
        let mut cfg = test_config();
        let extra: Vec<_> = (0..4)
            .map(|i| Candidate {
                id: format!("extra-{i}"),
                ..cfg.candidates[1].clone()
            })
            .collect();
        cfg.candidates.extend(extra);
        let r = ConfigRouter::new(cfg).with_mode(ClassifierMode::Disabled);
        let plan = r
            .plan(
                &profile("coding", DataClass::Public),
                None,
                &r.default_constraints(),
            )
            .expect("plan");
        assert!(
            plan.fallbacks.len() >= 3,
            "more candidates exist than attempts allowed"
        );
        assert_eq!(plan.attempt_order().len(), 3);
        let limiter = (0..3)
            .try_fold(AttemptLimiter::new(3), |l, _| l.begin_attempt())
            .expect("three allowed");
        assert_eq!(
            limiter.begin_attempt().expect_err("fourth").code,
            ErrorCode::BudgetExceeded
        );
    }

    #[test]
    fn test_plan_budget_too_small_returns_budget_exceeded_with_explanation() {
        let r = router(ClassifierMode::Disabled);
        let mut k = r.default_constraints();
        k.remaining_budget = Micros(1);
        let err = r
            .plan(&profile("coding", DataClass::Public), None, &k)
            .expect_err("no budget");
        assert_eq!(err.code, ErrorCode::BudgetExceeded);
        assert!(err.message.contains("exceeds remaining"));
    }

    #[test]
    fn test_plan_context_too_large_returns_context_overflow() {
        let r = router(ClassifierMode::Disabled);
        let mut p = profile("coding", DataClass::Public);
        p.est_input_tokens = 10_000_000;
        assert_eq!(
            r.select(&p, None).expect_err("too big").code,
            ErrorCode::ContextOverflow
        );
    }

    #[test]
    fn shadow_mode_never_changes_selection() {
        let p = profile("coding", DataClass::Public);
        let baseline = router(ClassifierMode::Disabled)
            .select(&p, None)
            .expect("baseline");
        let deep = classification("coding", "deep", 0.99);
        let plan = router(ClassifierMode::Shadow)
            .plan(
                &p,
                Some(&deep),
                &router(ClassifierMode::Shadow).default_constraints(),
            )
            .expect("plan");
        assert_eq!(plan.decision.model_id, baseline.model_id);
        let rec = plan.recommendation.expect("recorded");
        assert_eq!(rec.tier.as_deref(), Some("deep"));
        assert!(!rec.applied);
        assert_eq!(plan.decision.classifier_mode, "shadow");
    }

    #[test]
    fn test_plan_active_mode_approved_category_changes_tier() {
        let p = profile("coding", DataClass::Public);
        let deep = classification("coding", "deep", 0.99);
        let d = router(ClassifierMode::Active)
            .select(&p, Some(&deep))
            .expect("select");
        assert_eq!(d.model_id, "placeholder-deep-a");
        let research = classification("research", "deep", 0.99);
        let unapproved = router(ClassifierMode::Active)
            .select(&profile("research", DataClass::Public), Some(&research))
            .expect("select");
        assert_ne!(
            unapproved.model_id, "placeholder-deep-a",
            "category not approved"
        );
    }

    #[test]
    fn test_plan_active_mixed_uncertain_and_low_confidence_use_baseline() {
        let p = profile("coding", DataClass::Public);
        let base = router(ClassifierMode::Disabled)
            .select(&p, None)
            .expect("baseline")
            .model_id;
        for c in [
            classification("mixed", "deep", 0.99),
            classification("coding", "uncertain", 0.99),
            classification("coding", "deep", 0.5),
        ] {
            assert_eq!(
                router(ClassifierMode::Active)
                    .select(&p, Some(&c))
                    .expect("select")
                    .model_id,
                base
            );
        }
        let mut unknown_version = classification("coding", "deep", 0.99);
        unknown_version.model_version = "jev-9.9.9".into();
        assert_eq!(
            router(ClassifierMode::Active)
                .select(&p, Some(&unknown_version))
                .expect("select")
                .model_id,
            base
        );
    }

    #[test]
    fn confidence_cannot_authorize_action() {
        // Confidence 1.0 asks for the deep tier, but the deep model's provider is not allowed for
        // personal data in this config. High confidence must not widen the eligible set.
        let mut cfg = test_config();
        cfg.data_class_providers
            .insert(DataClass::Personal, vec!["ollama_cloud".into()]);
        cfg.classifier.active_categories = vec!["coding".into()];
        cfg.task_cap_micros = TEST_CAP_MICROS;
        let r = ConfigRouter::new(cfg).with_mode(ClassifierMode::Active);
        let p = profile("coding", DataClass::Personal);
        let sure = classification("coding", "deep", 1.0);
        let d = r.select(&p, Some(&sure)).expect("select");
        assert_ne!(d.model_id, "placeholder-deep-a");
        assert_eq!(d.model_id, "placeholder-strong-b");
        // And with nothing allowed at all, confidence cannot conjure a model.
        let mut none = test_config();
        none.data_class_providers
            .insert(DataClass::Personal, vec![]);
        none.classifier.active_categories = vec!["coding".into()];
        let r = ConfigRouter::new(none).with_mode(ClassifierMode::Active);
        assert!(r.select(&p, Some(&sure)).is_err());
    }
}
