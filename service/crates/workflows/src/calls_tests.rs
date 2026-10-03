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
    // 4 input bytes at 3 USD/Mtok = 12 micros; 1000 output tokens at 15 USD/Mtok = 15_000
    assert_eq!(reservations[0].max_cost, Micros(15_012));
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
