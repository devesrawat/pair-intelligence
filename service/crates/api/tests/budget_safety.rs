//! The adapter budget routes never trust the caller's numbers: the server derives the hold from the
//! registry price, bounds what a settlement may claim and binds a reservation to its task.

mod common;

use axum::http::StatusCode;
use common::stack::{Stack, StackOpts, MID, PREMIUM, PRICE_VERSION};
use common::{post_json, send};
use serde_json::{json, Value};

const TASK: &str = "0199c0de-1234-7abc-8def-0123456789ab";
const OTHER_TASK: &str = "0199c0de-9999-7abc-8def-0123456789ab";

fn reserve_body(model: Option<&str>, kind: &str, max_cost_micros: i64) -> Value {
    let mut body = json!({
        "task_id": TASK,
        "kind": kind,
        "category": "metered",
        "max_cost_micros": max_cost_micros,
        "price_version": PRICE_VERSION,
    });
    if let Some(model) = model {
        body["model_id"] = json!(model);
    }
    body
}

fn settle(reservation: &str, task: &str, tokens: (u64, u64), cost: Value) -> Value {
    json!({
        "reservation_id": reservation,
        "task_id": task,
        "input_tokens": tokens.0,
        "output_tokens": tokens.1,
        "actual_cost_micros": cost,
        "price_version": PRICE_VERSION,
    })
}

async fn reserve(stack: &Stack, body: &Value) -> (StatusCode, Value) {
    let (resp, out) = send(&stack.app, post_json("/v1/budget/reserve", body)).await;
    (resp.status(), out)
}

async fn reserved_id(stack: &Stack, body: &Value) -> String {
    let (status, out) = reserve(stack, body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    out["data"]["reservation_id"]
        .as_str()
        .expect("reservation id")
        .to_owned()
}

async fn row(stack: &Stack, id: &str) -> (String, i64) {
    sqlx::query_as("SELECT state, counted_micros FROM budget_reservations WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&stack.db.pool)
        .await
        .expect("reservation row")
}

fn worst_case(stack: &Stack, model: &str) -> i64 {
    let entry = stack.services.registry.get(model).expect("model");
    let price = entry.price.as_ref().expect("price");
    let floor = stack.services.adapter_budget.reserve_floor_input_tokens;
    price
        .max_cost(floor.min(entry.context_tokens), entry.max_output_tokens)
        .expect("cost")
        .0
}

#[tokio::test]
async fn reserve_hold_is_at_least_registry_worst_case() {
    let stack = Stack::start(StackOpts::default()).await;
    let worst = worst_case(&stack, MID);
    assert!(worst > 1, "the test needs a real floor");
    // The client asks for a one-micro hold: the server hold is the registry worst case.
    let id = reserved_id(&stack, &reserve_body(Some(MID), "coding", 1)).await;
    assert_eq!(row(&stack, &id).await, ("held".to_owned(), worst));

    // Without a model id the worst case over every model priced under the version applies.
    let mut body = reserve_body(None, "coding", 1);
    body["task_id"] = json!(OTHER_TASK);
    let id = reserved_id(&stack, &body).await;
    assert_eq!(row(&stack, &id).await.1, worst_case(&stack, PREMIUM));

    // The client may raise the hold, never lower it.
    let mut body = reserve_body(Some(MID), "coding", worst + 5_000);
    body["task_id"] = json!("0199c0de-aaaa-7abc-8def-0123456789ab");
    let id = reserved_id(&stack, &body).await;
    assert_eq!(row(&stack, &id).await.1, worst + 5_000);
    stack.finish().await;
}

#[tokio::test]
async fn reserve_unknown_model_refused() {
    let stack = Stack::start(StackOpts::default()).await;
    let (status, out) = reserve(
        &stack,
        &reserve_body(Some("no-such-model"), "coding", 50_000),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(out["error"]["code"], "budget_unknown_price");
    // A price version that is not the model's own is refused as well.
    let mut body = reserve_body(Some(MID), "coding", 50_000);
    body["price_version"] = json!("some-other-version");
    let (status, out) = reserve(&stack, &body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM budget_reservations")
        .fetch_one(&stack.db.pool)
        .await
        .expect("count");
    assert_eq!(rows, 0);
    stack.finish().await;
}

#[tokio::test]
async fn reserve_requires_task_id() {
    let stack = Stack::start(StackOpts::default()).await;
    let mut body = reserve_body(Some(MID), "coding", 50_000);
    body.as_object_mut().expect("object").remove("task_id");
    let (status, out) = reserve(&stack, &body).await;
    assert!(status.is_client_error(), "{status}: {out}");
    assert_eq!(out["error"]["code"], "invalid_input");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM budget_reservations")
        .fetch_one(&stack.db.pool)
        .await
        .expect("count");
    assert_eq!(
        rows, 0,
        "no anonymous reservation that no task cap applies to"
    );
    stack.finish().await;
}

#[tokio::test]
async fn reconcile_cannot_underreport_cost() {
    let stack = Stack::start(StackOpts::default()).await;
    let id = reserved_id(&stack, &reserve_body(Some(MID), "coding", 1)).await;
    // 100k in x $2/M + 10k out x $10/M = 300_000 micros, whatever the caller claims.
    let body = settle(&id, TASK, (100_000, 10_000), json!(1));
    let (resp, out) = send(&stack.app, post_json("/v1/budget/reconcile", &body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{out}");
    assert_eq!(out["data"]["amount_micros"], 300_000);
    assert_eq!(out["data"]["state"], "settled");
    assert_eq!(row(&stack, &id).await, ("settled".to_owned(), 300_000));
    stack.finish().await;
}

#[tokio::test]
async fn reconcile_rejects_i64_max_actual_cost_and_budget_still_works() {
    let stack = Stack::start(StackOpts::default()).await;
    let id = reserved_id(&stack, &reserve_body(Some(MID), "coding", 1)).await;
    let held = row(&stack, &id).await;
    for cost in [
        json!(i64::MAX),
        json!(i64::MAX - 1),
        json!(10_000_000_000_000_i64),
    ] {
        let body = settle(&id, TASK, (10, 10), cost.clone());
        let (resp, out) = send(&stack.app, post_json("/v1/budget/reconcile", &body)).await;
        assert!(resp.status().is_client_error(), "{cost}: {out}");
        assert_eq!(
            row(&stack, &id).await,
            held,
            "a refused report changes nothing"
        );
    }
    // Far above reserved x overrun factor, but below the hard ceiling: refused as well.
    let body = settle(&id, TASK, (10, 10), json!(held.1 * 100));
    let (resp, _) = send(&stack.app, post_json("/v1/budget/reconcile", &body)).await;
    assert!(resp.status().is_client_error());

    // The ledger still works: another task reserves, and the honest report settles.
    let mut next = reserve_body(Some(MID), "coding", 1);
    next["task_id"] = json!(OTHER_TASK);
    reserved_id(&stack, &next).await;
    let body = settle(&id, TASK, (100, 20), json!(500));
    let (resp, out) = send(&stack.app, post_json("/v1/budget/reconcile", &body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{out}");
    stack.finish().await;
}

#[tokio::test]
async fn reconcile_with_wrong_task_is_refused() {
    let stack = Stack::start(StackOpts::default()).await;
    let id = reserved_id(&stack, &reserve_body(Some(MID), "coding", 1)).await;
    let held = row(&stack, &id).await;
    let body = settle(&id, OTHER_TASK, (100, 20), json!(500));
    let (resp, out) = send(&stack.app, post_json("/v1/budget/reconcile", &body)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{out}");
    assert_eq!(row(&stack, &id).await, held);

    let mut missing = settle(&id, TASK, (100, 20), json!(500));
    missing.as_object_mut().expect("object").remove("task_id");
    let (resp, _) = send(&stack.app, post_json("/v1/budget/reconcile", &missing)).await;
    assert!(resp.status().is_client_error(), "task_id is required");

    let body = settle(&id, TASK, (100, 20), json!(500));
    let (resp, out) = send(&stack.app, post_json("/v1/budget/reconcile", &body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{out}");
    stack.finish().await;
}

#[tokio::test]
async fn reconcile_with_zero_reported_cost_is_counted_at_the_token_price() {
    let stack = Stack::start(StackOpts::default()).await;
    let id = reserved_id(&stack, &reserve_body(Some(MID), "coding", 1)).await;
    // 100 in x $2/M + 20 out x $10/M = 400 micros: a reported 0 does not make the call free.
    let body = settle(&id, TASK, (100, 20), json!(0));
    let (resp, out) = send(&stack.app, post_json("/v1/budget/reconcile", &body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{out}");
    assert_eq!(out["data"]["amount_micros"], 400);
    assert_eq!(row(&stack, &id).await, ("settled".to_owned(), 400));
    stack.finish().await;
}
