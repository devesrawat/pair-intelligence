//! Replays of one logical turn (same conversation and `client_message_id`): one paid call, a stored
//! answer once finished, and a refusal when the content changed.

mod common;

use axum::http::StatusCode;
use common::mock::ProviderMode;
use common::stack::{Stack, StackOpts};
use common::{post_json, send};
use pair_core::ids::TraceId;
use pair_core::types::DataClass;
use serde_json::{json, Value};

const SLOW_MS: u64 = 600;

async fn conversation(stack: &Stack) -> String {
    stack
        .services
        .store
        .create_conversation_with_class("replay", TraceId::new(), DataClass::Public)
        .await
        .expect("conversation")
        .to_string()
}

fn body(conversation: &str, message: &str) -> Value {
    json!({
        "conversation_id": conversation,
        "client_message_id": "turn-1",
        "message": message,
        "data_class": "public",
    })
}

async fn count(stack: &Stack, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&stack.db.pool)
        .await
        .expect("count query")
}

#[tokio::test]
async fn concurrent_identical_turns_pay_once() {
    let stack = Stack::start(StackOpts {
        provider: ProviderMode::Slow {
            text: "paid once".into(),
            delay_ms: SLOW_MS,
        },
        ..StackOpts::default()
    })
    .await;
    let conversation = conversation(&stack).await;
    let request = body(&conversation, "hello");
    let (first, second) = tokio::join!(
        send(&stack.app, post_json("/v1/turn", &request)),
        send(&stack.app, post_json("/v1/turn", &request)),
    );
    let mut statuses = [first.0.status(), second.0.status()];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "{} / {}",
        first.1,
        second.1
    );
    assert_eq!(
        stack.provider.hits(),
        1,
        "two identical turns, one paid call"
    );
    assert_eq!(count(&stack, "SELECT count(*) FROM model_calls").await, 1);
    assert_eq!(
        count(&stack, "SELECT count(*) FROM budget_reservations").await,
        1
    );
    assert_eq!(
        count(
            &stack,
            "SELECT count(*) FROM messages WHERE role = 'assistant'"
        )
        .await,
        1
    );

    // Once the winner has finished, the same request gets its stored answer.
    let (resp, again) = send(&stack.app, post_json("/v1/turn", &request)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{again}");
    assert_eq!(again["data"]["text"], "paid once");
    assert_eq!(stack.provider.hits(), 1);
    stack.finish().await;
}

#[tokio::test]
async fn replay_with_changed_content_is_refused() {
    let stack = Stack::start(StackOpts::default()).await;
    let conversation = conversation(&stack).await;
    let (resp, out) = send(
        &stack.app,
        post_json("/v1/turn", &body(&conversation, "original question")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "{out}");

    let changed = body(&conversation, "something else entirely");
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &changed)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "{out}");
    assert_eq!(stack.provider.hits(), 1, "the new text is never processed");
    let contents: Vec<String> =
        sqlx::query_scalar("SELECT content FROM messages WHERE role = 'user'")
            .fetch_all(&stack.db.pool)
            .await
            .expect("user messages");
    assert_eq!(
        contents,
        ["original question"],
        "the audit row keeps the original text"
    );
    assert_eq!(count(&stack, "SELECT count(*) FROM model_calls").await, 1);
    stack.finish().await;
}

#[tokio::test]
async fn replay_with_changed_content_after_a_failed_turn_is_refused() {
    let stack = Stack::start(StackOpts {
        provider: ProviderMode::Fail(500),
        ..StackOpts::default()
    })
    .await;
    let conversation = conversation(&stack).await;
    let (resp, _) = send(
        &stack.app,
        post_json("/v1/turn", &body(&conversation, "original question")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let hits = stack.provider.hits();
    let changed = body(&conversation, "different text under the same id");
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &changed)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "{out}");
    assert_eq!(stack.provider.hits(), hits);
    stack.finish().await;
}

#[tokio::test]
async fn replay_of_finished_turn_returns_stored_answer() {
    let stack = Stack::start(StackOpts::default()).await;
    let conversation = conversation(&stack).await;
    let request = body(&conversation, "hello");
    let (resp, first) = send(&stack.app, post_json("/v1/turn", &request)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{first}");
    assert_eq!(first["data"]["replayed"], false);

    let (resp, again) = send(&stack.app, post_json("/v1/turn", &request)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{again}");
    assert_eq!(again["data"]["replayed"], true);
    for field in [
        "text",
        "message_id",
        "conversation_id",
        "resolved_model",
        "route_reason",
        "cost_state",
    ] {
        assert_eq!(again["data"][field], first["data"][field], "{field}");
    }
    assert_eq!(
        stack.provider.hits(),
        1,
        "a replay never pays for a second answer"
    );
    assert_eq!(
        count(&stack, "SELECT count(*) FROM messages").await,
        2,
        "no new messages"
    );
    stack.finish().await;
}
