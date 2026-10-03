//! A turn outlives its HTTP request: it keeps its in-flight permit, shutdown waits for it, and a
//! turn that is cancelled between reserve and reconcile leaves an unresolved (never `held`)
//! reservation.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::mock::ProviderMode;
use common::stack::{Stack, StackOpts};
use common::{authed, post_json, send};
use pair_api::limits::Limits;
use pair_api::shutdown::drain_turns;
use pair_api::turn::{run_turn, ValidTurn};
use pair_core::ids::TraceId;
use pair_core::types::{DataClass, TaskKind};
use serde_json::json;

const SLOW_MS: u64 = 700;
const POLL: Duration = Duration::from_millis(20);
const WAIT: Duration = Duration::from_secs(10);

fn slow() -> ProviderMode {
    ProviderMode::Slow {
        text: "late answer".into(),
        delay_ms: SLOW_MS,
    }
}

async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(POLL).await;
    }
}

fn turn_body() -> serde_json::Value {
    json!({ "message": "hello", "data_class": "public" })
}

#[tokio::test]
async fn disconnect_mid_turn_does_not_exceed_in_flight_limit() {
    let stack = Stack::start(StackOpts {
        provider: slow(),
        limits: Some(Limits {
            request_timeout: Duration::from_millis(150),
            max_in_flight: 1,
        }),
        ..StackOpts::default()
    })
    .await;
    // The HTTP timeout cuts the request off while the detached turn is still waiting on the provider.
    let (resp, _) = send(&stack.app, post_json("/v1/turn", &turn_body())).await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(stack.provider.hits(), 1);

    // The turn still occupies the only in-flight slot: a second request is shed, not run beside it.
    let (resp, _) = send(&stack.app, authed("/v1/whoami")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let (resp, _) = send(&stack.app, post_json("/v1/turn", &turn_body())).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        stack.provider.hits(),
        1,
        "no second turn beside the detached one"
    );

    // Once it finishes the slot is free again.
    let turns = stack.services.turns.clone();
    until("the detached turn finishes", || turns.is_empty()).await;
    let (resp, _) = send(&stack.app, authed("/v1/whoami")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    stack.finish().await;
}

#[tokio::test]
async fn shutdown_waits_for_detached_turns() {
    let stack = Stack::start(StackOpts {
        provider: slow(),
        ..StackOpts::default()
    })
    .await;
    let app = stack.app.clone();
    let client = tokio::spawn(async move {
        send(&app, post_json("/v1/turn", &turn_body())).await;
    });
    let provider = stack.provider.clone();
    until("the provider is called", move || provider.hits() == 1).await;
    client.abort();

    // The client is gone but the turn is not: a short drain gives up, a long one waits it out.
    let turns = stack.services.turns.clone();
    assert!(
        !drain_turns(&turns, Duration::from_millis(100)).await,
        "the turn is still running"
    );
    assert!(
        drain_turns(&turns, WAIT).await,
        "the drain waits for the turn"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM model_calls c JOIN messages m ON m.id = c.response_message_id \
         WHERE m.role = 'assistant' AND c.cost_state = 'reconciled'",
    )
    .fetch_one(&stack.db.pool)
    .await
    .expect("count");
    assert_eq!(rows, 1, "the detached turn finished and was recorded");
    stack.finish().await;
}

#[tokio::test]
async fn cancelled_turn_leaves_unresolved_reservation_and_a_retryable_claim() {
    let stack = Stack::start(StackOpts {
        provider: slow(),
        ..StackOpts::default()
    })
    .await;
    let conversation = stack
        .services
        .store
        .create_conversation_with_class("cancel", TraceId::new(), DataClass::Public)
        .await
        .expect("conversation");
    let services = stack.services.clone();
    let turn = ValidTurn {
        conversation: Some(conversation),
        client_message_id: Some("turn-1".into()),
        message: "hello".into(),
        data_class: DataClass::Public,
        kind: TaskKind::Default,
        project_type: None,
    };
    let task =
        tokio::spawn(async move { run_turn(&services, "tester", TraceId::new(), turn).await });
    let provider = stack.provider.clone();
    until("the provider is called", move || provider.hits() == 1).await;
    task.abort();
    // Joining the aborted task is what guarantees its future (and the guard in it) was dropped.
    assert!(task.await.expect_err("cancelled").is_cancelled());

    assert!(drain_turns(&stack.services.turns, WAIT).await);
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM budget_reservations")
        .fetch_all(&stack.db.pool)
        .await
        .expect("reservations");
    assert_eq!(states, ["unresolved"], "never left `held`");
    let claim: String = sqlx::query_scalar("SELECT state FROM turn_claims")
        .fetch_one(&stack.db.pool)
        .await
        .expect("claim");
    assert_eq!(claim, "failed", "the retry is not blocked by a dead claim");
    stack.finish().await;
}
