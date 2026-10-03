//! What docs/runbooks/kill-switch.md claims for levels 1 and 3, proven against the hosted service.
//! Level 2 (interrupting in-flight jobs) has no test because it has no mechanism: it stays
//! NOT EFFECTIVE in the runbook.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::stack::{Stack, StackOpts, CHEAP, PRICE_VERSION};
use common::{post_json, send};
use serde_json::{json, Value};

fn turn(message: &str, class: &str) -> Value {
    json!({ "message": message, "data_class": class })
}

/// The runbook's level-1 step, performed on the real shipped file: every cap set to 0.00.
fn zeroed_shipped_budget() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/budget.yaml");
    let text = std::fs::read_to_string(path).expect("shipped budget.yaml");
    let caps = [
        "metered_monthly_cap",
        "metered_daily_cap",
        "classifier_monthly_subcap",
        "default_task_cap",
        "research_task_cap",
        "coding_task_cap",
    ];
    text.lines()
        .map(|line| {
            let key = line.trim_start().split(':').next().unwrap_or_default();
            if caps.contains(&key) {
                format!("  {key}: 0.00")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn zeroing_the_shipped_budget_yaml_as_the_runbook_says_refuses_turns_and_adapter_reserves() {
    let zeroed = zeroed_shipped_budget();
    pair_budget::BudgetConfig::from_yaml_strict(&zeroed)
        .expect("all-zero caps pass the strict validation the service applies at startup");
    let stack = Stack::start(StackOpts {
        budget_yaml: zeroed,
        ..StackOpts::default()
    })
    .await;
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hello", "public"))).await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "{body}");
    let reserve = json!({
        "task_id": "0199c0de-1234-7abc-8def-0123456789ab",
        "model_id": CHEAP,
        "kind": "default",
        "category": "metered",
        "max_cost_micros": 1,
        "price_version": PRICE_VERSION,
    });
    let (resp, body) = send(&stack.app, post_json("/v1/budget/reserve", &reserve)).await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(stack.provider.hits(), 0);
    stack.finish().await;
}

#[tokio::test]
async fn rotated_service_token_locks_out_the_old_token() {
    let stack = Stack::start(StackOpts::default()).await;
    let rotated = pair_api::state::AppState::new(stack.db.pool.clone(), "rotated-token-0123456789")
        .with_services(stack.services.clone());
    let app = pair_api::router(rotated);
    // The old token no longer reaches any /v1 endpoint ...
    let (resp, _) = send(&app, post_json("/v1/turn", &turn("hello", "public"))).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let (resp, _) = send(&app, post_json("/v1/budget/reserve", &json!({}))).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // ... and the new one does.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/turn")
        .header("authorization", "Bearer rotated-token-0123456789")
        .header("x-actor", "tester")
        .header("content-type", "application/json")
        .body(Body::from(turn("payroll", "employer").to_string()))
        .expect("request");
    let (resp, _) = send(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "authenticated, then refused by the data-class gate"
    );
    assert_eq!(stack.provider.hits(), 0);
    stack.finish().await;
}
