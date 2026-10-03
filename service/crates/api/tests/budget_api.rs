//! `POST /v1/budget/reserve` and `/v1/budget/reconcile`: the OpenClaw adapter's budget surface.

mod common;

use axum::http::StatusCode;
use common::stack::{Stack, StackOpts, CHEAP, PRICE_VERSION};
use common::{post_json, send};
use serde_json::{json, Value};

const TASK: &str = "0199c0de-1234-7abc-8def-0123456789ab";

fn reserve_body(kind: &str, max_cost_micros: i64) -> Value {
    json!({
        "task_id": TASK,
        "model_id": CHEAP,
        "kind": kind,
        "category": "metered",
        "max_cost_micros": max_cost_micros,
        "price_version": PRICE_VERSION,
    })
}

fn settle_body(reservation: &str, cost: Value) -> Value {
    json!({
        "reservation_id": reservation,
        "task_id": TASK,
        "input_tokens": 100,
        "output_tokens": 20,
        "actual_cost_micros": cost,
        "price_version": PRICE_VERSION,
    })
}

async fn reservation(stack: &Stack, id: &str) -> (String, i64) {
    sqlx::query_as("SELECT state, counted_micros FROM budget_reservations WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&stack.db.pool)
        .await
        .expect("reservation row")
}

#[tokio::test]
async fn adapter_reserve_then_reconcile_roundtrip() {
    let stack = Stack::start(StackOpts::default()).await;
    // A client figure above the server's worst case is honoured: it may only raise the hold.
    let (resp, body) = send(
        &stack.app,
        post_json("/v1/budget/reserve", &reserve_body("coding", 150_000)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    let id = body["data"]["reservation_id"]
        .as_str()
        .expect("reservation id")
        .to_owned();
    assert_eq!(body["data"]["task_id"], TASK);
    assert_eq!(body["data"]["max_cost_micros"], 150_000);
    assert_eq!(reservation(&stack, &id).await, ("held".to_owned(), 150_000));

    let settle = settle_body(&id, json!(1_234));
    let (resp, body) = send(&stack.app, post_json("/v1/budget/reconcile", &settle)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(body["data"]["settled"], true);
    assert_eq!(body["data"]["state"], "settled");
    assert_eq!(body["data"]["amount_micros"], 1_234);
    assert_eq!(
        reservation(&stack, &id).await,
        ("settled".to_owned(), 1_234)
    );

    // A replay of the same report returns the same ledger entry.
    let (_, again) = send(&stack.app, post_json("/v1/budget/reconcile", &settle)).await;
    assert_eq!(again["data"]["entry_id"], body["data"]["entry_id"]);
    stack.finish().await;
}

#[tokio::test]
async fn reserve_over_cap_returns_budget_exceeded_and_reserves_nothing() {
    let stack = Stack::start(StackOpts::default()).await;
    // The default task cap is $0.10.
    let (resp, body) = send(
        &stack.app,
        post_json("/v1/budget/reserve", &reserve_body("default", 150_000)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(body["error"]["code"], "budget_exceeded");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM budget_reservations")
        .fetch_one(&stack.db.pool)
        .await
        .expect("count");
    assert_eq!(rows, 0, "a refused reservation must leave no row behind");
    stack.finish().await;
}

#[tokio::test]
async fn reserve_with_unknown_price_version_is_budget_unknown_price() {
    let stack = Stack::start(StackOpts::default()).await;
    let mut req = reserve_body("default", 10_000);
    req["price_version"] = json!("no-such-version");
    let (resp, body) = send(&stack.app, post_json("/v1/budget/reserve", &req)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "budget_unknown_price");
    stack.finish().await;
}

#[tokio::test]
async fn reserve_requires_explicit_kind_and_category() {
    let stack = Stack::start(StackOpts::default()).await;
    for missing in [
        "kind",
        "category",
        "price_version",
        "max_cost_micros",
        "task_id",
    ] {
        let mut req = reserve_body("default", 10_000);
        req.as_object_mut().expect("object").remove(missing);
        let (resp, body) = send(&stack.app, post_json("/v1/budget/reserve", &req)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{missing}: {body}");
        assert_eq!(body["error"]["code"], "invalid_input", "{missing}");
    }
    let (resp, _) = send(
        &stack.app,
        post_json("/v1/budget/reserve", &reserve_body("default", 0)),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "zero cost is not a reservation"
    );
    stack.finish().await;
}

#[tokio::test]
async fn reconcile_unknown_cost_is_unresolved() {
    let stack = Stack::start(StackOpts::default()).await;
    let (_, body) = send(
        &stack.app,
        post_json("/v1/budget/reserve", &reserve_body("default", 40_000)),
    )
    .await;
    let id = body["data"]["reservation_id"]
        .as_str()
        .expect("reservation id")
        .to_owned();
    let held = reservation(&stack, &id).await.1;
    let settle = settle_body(&id, Value::Null);
    let (resp, body) = send(&stack.app, post_json("/v1/budget/reconcile", &settle)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(body["data"]["settled"], false);
    assert_eq!(body["data"]["state"], "unresolved");
    assert_eq!(
        reservation(&stack, &id).await,
        ("unresolved".to_owned(), held),
        "never assumed zero"
    );
    stack.finish().await;
}

#[tokio::test]
async fn reconcile_unknown_reservation_is_not_found() {
    let stack = Stack::start(StackOpts::default()).await;
    let settle = settle_body(TASK, json!(1));
    let (resp, body) = send(&stack.app, post_json("/v1/budget/reconcile", &settle)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{body}");
    stack.finish().await;
}
