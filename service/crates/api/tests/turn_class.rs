//! A conversation's data class can only rise: a later turn may never relabel earlier history.

mod common;

use axum::http::StatusCode;
use common::stack::{Stack, StackOpts};
use common::{post_json, send};
use pair_core::ids::TraceId;
use pair_core::types::DataClass;
use serde_json::{json, Value};

fn turn(message: &str, class: &str) -> Value {
    json!({ "message": message, "data_class": class })
}

async fn stored_class(stack: &Stack, conversation: &str) -> String {
    sqlx::query_scalar("SELECT data_class FROM conversations WHERE id = $1::uuid")
        .bind(conversation)
        .fetch_one(&stack.db.pool)
        .await
        .expect("conversation row")
}

async fn count(stack: &Stack, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&stack.db.pool)
        .await
        .expect("count query")
}

#[tokio::test]
async fn new_conversation_records_declared_class() {
    let stack = Stack::start(StackOpts::default()).await;
    for class in ["public", "personal"] {
        let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hello", class))).await;
        assert_eq!(resp.status(), StatusCode::OK, "{class}: {body}");
        let conversation = body["data"]["conversation_id"].as_str().expect("id");
        assert_eq!(stored_class(&stack, conversation).await, class);
    }
    stack.finish().await;
}

#[tokio::test]
async fn turn_cannot_lower_conversation_data_class() {
    let stack = Stack::start(StackOpts::default()).await;
    let (_, first) = send(
        &stack.app,
        post_json("/v1/turn", &turn("my medical history", "personal")),
    )
    .await;
    let conversation = first["data"]["conversation_id"].as_str().expect("id");
    let messages_before = count(&stack, "SELECT count(*) FROM messages").await;
    let reservations_before = count(&stack, "SELECT count(*) FROM budget_reservations").await;

    let mut lower = turn("summarise the conversation so far", "public");
    lower["conversation_id"] = json!(conversation);
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &lower)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "policy_denied");
    assert_eq!(
        stack.provider.hits(),
        1,
        "the refused turn never reaches a provider"
    );
    assert_eq!(
        count(&stack, "SELECT count(*) FROM messages").await,
        messages_before,
        "nothing is written for a refused turn"
    );
    assert_eq!(
        count(&stack, "SELECT count(*) FROM budget_reservations").await,
        reservations_before
    );
    assert_eq!(stored_class(&stack, conversation).await, "personal");
    stack.finish().await;
}

#[tokio::test]
async fn turn_at_or_above_the_stored_class_is_accepted_and_raises_it() {
    let stack = Stack::start(StackOpts::default()).await;
    let conversation = stack
        .services
        .store
        .create_conversation_with_class("raise", TraceId::new(), DataClass::Public)
        .await
        .expect("conversation");
    for (class, expected) in [("public", "public"), ("personal", "personal")] {
        let mut body = turn("hello", class);
        body["conversation_id"] = json!(conversation.0);
        let (resp, out) = send(&stack.app, post_json("/v1/turn", &body)).await;
        assert_eq!(resp.status(), StatusCode::OK, "{class}: {out}");
        assert_eq!(
            stored_class(&stack, &conversation.to_string()).await,
            expected
        );
    }
    stack.finish().await;
}

#[tokio::test]
async fn existing_conversations_default_to_personal() {
    let stack = Stack::start(StackOpts::default()).await;
    let conversation = stack
        .services
        .store
        .create_conversation("legacy", TraceId::new())
        .await
        .expect("conversation");
    assert_eq!(
        stored_class(&stack, &conversation.to_string()).await,
        "personal"
    );
    let mut body = turn("hello", "public");
    body["conversation_id"] = json!(conversation.0);
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &body)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{out}");
    stack.finish().await;
}

/// Another turn raises the class between this turn's check and its history read. The history it
/// reads then holds more sensitive text than this turn declared, so the call must go out under the
/// conversation's class. The trigger plays that other turn.
#[tokio::test]
async fn history_sent_under_highest_class_in_conversation() {
    let stack = Stack::start(StackOpts {
        public_only_models: true,
        ..StackOpts::default()
    })
    .await;
    let conversation = stack
        .services
        .store
        .create_conversation_with_class("race", TraceId::new(), DataClass::Public)
        .await
        .expect("conversation");
    let mut body = turn("hello", "public");
    body["conversation_id"] = json!(conversation.0);
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "control: {out}");
    assert_eq!(stack.provider.hits(), 1);

    sqlx::query(
        "CREATE FUNCTION pair_test_raise() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         UPDATE conversations SET data_class = 'personal' WHERE id = NEW.conversation_id; \
         RETURN NEW; END $$",
    )
    .execute(&stack.db.pool)
    .await
    .expect("function");
    sqlx::query(
        "CREATE TRIGGER pair_test_raise AFTER INSERT ON messages \
         FOR EACH ROW EXECUTE FUNCTION pair_test_raise()",
    )
    .execute(&stack.db.pool)
    .await
    .expect("trigger");

    let mut again = turn("and again", "public");
    again["conversation_id"] = json!(conversation.0);
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &again)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{out}");
    assert_eq!(out["error"]["code"], "provider_disallowed");
    assert_eq!(
        stack.provider.hits(),
        1,
        "personal history must not reach a public-only model"
    );
    stack.finish().await;
}
