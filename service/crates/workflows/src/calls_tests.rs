#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::calls::budgeted_generate;
use crate::coding::testkit::{FakeBudget, FixedPrices, FnProvider, TEST_PRICE_VERSION};
use pair_core::{
    error::{ErrorCode, PairError},
    ids::{TaskId, TraceId},
    money::Micros,
    types::{DataClass, ModelMessage, ModelRequest, TaskKind, TrustClass},
};
use std::sync::atomic::Ordering;

fn request(content: &str, max_out: u32, class: DataClass) -> ModelRequest {
    ModelRequest {
        model_id: "fake".into(),
        messages: vec![ModelMessage {
            role: "user".into(),
            content: content.into(),
            trust: TrustClass::Owner,
        }],
        max_output_tokens: max_out,
        deadline_ms: 1000,
        data_class: class,
        task: TaskId::new(),
        trace: TraceId::new(),
    }
}

#[tokio::test]
async fn failed_call_leaves_reservation_unresolved() {
    let budget = FakeBudget::default();
    let provider = FnProvider::new(Box::new(|_, _| {
        Err(PairError::new(ErrorCode::ProviderUnavailable, "boom"))
    }));
    let err = budgeted_generate(
        &provider,
        &budget,
        &FixedPrices::standard(),
        TaskKind::Coding,
        request("hi", 10, DataClass::Personal),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::ProviderUnavailable);
    let usages = budget.usages.lock().unwrap();
    assert_eq!(usages.len(), 1);
    assert_eq!(usages[0].actual_cost, None, "must never assume zero cost");
    assert_eq!(usages[0].price_version, TEST_PRICE_VERSION);
}

#[tokio::test]
async fn reservation_derived_from_price_and_max_output() {
    let budget = FakeBudget::default();
    let provider = FnProvider::scripted(vec!["ok".into()]);
    budgeted_generate(
        &provider,
        &budget,
        &FixedPrices::standard(),
        TaskKind::Research,
        request("abcd", 1000, DataClass::Public),
    )
    .await
    .unwrap();
    let reservations = budget.reservations.lock().unwrap();
    // 4 bytes = 2 reserved tokens at 3 USD/Mtok = 6 micros; 1000 output tokens at 15 USD/Mtok = 15_000
    assert_eq!(reservations[0].max_cost, Micros(15_006));
    assert_eq!(reservations[0].kind, TaskKind::Research);
    assert_eq!(
        reservations[0].price_version.as_deref(),
        Some(TEST_PRICE_VERSION)
    );
}

#[tokio::test]
async fn unknown_price_never_reaches_the_provider() {
    let budget = FakeBudget::default();
    let provider = FnProvider::scripted(vec!["ok".into()]);
    let err = budgeted_generate(
        &provider,
        &budget,
        &FixedPrices(None),
        TaskKind::Coding,
        request("x", 10, DataClass::Public),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::BudgetUnknownPrice);
    assert_eq!(provider.call_count(), 0);
    assert_eq!(budget.reserved.load(Ordering::SeqCst), 0);
}

const MODELS_YAML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/models.yaml");

fn profile(intent: &str, class: DataClass, tokens: u64) -> pair_core::types::TaskProfile {
    pair_core::types::TaskProfile {
        intent: intent.into(),
        difficulty: "substantial".into(),
        data_class: class,
        needs_tools: false,
        est_input_tokens: tokens,
    }
}

fn real_router() -> pair_models::router::ConfigRouter {
    use pair_models::{classification::config::RoutingConfig, router::ConfigRouter};
    ConfigRouter::new(RoutingConfig::from_path(std::path::Path::new(MODELS_YAML)).unwrap())
}

/// The real router, not the test double: `config/models.yaml` must route both workflows at a
/// realistic (100k-token) context, within the attempt limit.
#[test]
fn real_router_serves_both_workflows_at_a_realistic_context() {
    use crate::calls::ModelPlanner;
    let router = real_router();
    for (intent, class) in [
        ("coding", DataClass::Personal),
        ("research", DataClass::Public),
    ] {
        let order = router
            .attempt_order(&profile(intent, class, 100_000))
            .unwrap_or_else(|e| panic!("{intent}: {e}"));
        assert!(!order.is_empty() && order.len() <= crate::calls::MAX_MODEL_ATTEMPTS);
    }
}

/// KNOWN CONFIG GAP (owner: config/models): the routing candidates in `config/models.yaml`
/// (`placeholder-strong-a`, ...) are not in the provider registry and the only registry-known
/// candidate (`gemma4:31b`) has no price, so `budgeted_generate` refuses every routed model.
/// Run with `--ignored` to see the current state; it passes once the ids and prices line up.
#[test]
#[ignore = "config/models.yaml routing candidates are not priced registry models"]
fn registry_prices_every_model_the_router_can_return() {
    use crate::calls::{ModelPlanner, PriceSource};
    use pair_models::provider::ProviderRegistry;
    let registry = ProviderRegistry::load(std::path::Path::new(MODELS_YAML)).unwrap();
    let router = real_router();
    for (intent, class) in [
        ("coding", DataClass::Personal),
        ("research", DataClass::Public),
    ] {
        for id in router
            .attempt_order(&profile(intent, class, 2_000))
            .unwrap()
        {
            let terms = registry
                .terms(&id)
                .unwrap_or_else(|| panic!("{id} is not in the registry"));
            assert!(terms.price.is_some(), "{id} has no price");
            assert!(terms.allowed_data_classes.contains(&class), "{id}");
        }
    }
}
