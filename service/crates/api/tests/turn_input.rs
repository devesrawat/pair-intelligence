//! Request-boundary hardening for `POST /v1/turn`: control characters and error bodies.

mod common;

use axum::http::StatusCode;
use common::stack::{Stack, StackOpts};
use common::{post_json, send};
use serde_json::{json, Value};

fn turn(message: &str) -> Value {
    json!({ "message": message, "data_class": "public" })
}

async fn count(stack: &Stack, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&stack.db.pool)
        .await
        .expect("count query")
}

#[tokio::test]
async fn nul_in_message_is_422_not_500() {
    let stack = Stack::start(StackOpts::default()).await;
    for message in ["a\u{0}b", "bell\u{7}", "esc\u{1b}[31m", "del\u{7f}"] {
        let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn(message))).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{message:?}: {body}"
        );
        assert_eq!(body["error"]["code"], "invalid_input");
    }
    let mut body = turn("hello");
    body["project_type"] = json!("web\u{0}app");
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &body)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(stack.provider.hits(), 0);
    assert_eq!(
        count(&stack, "conversations").await,
        0,
        "refused before any write"
    );
    assert_eq!(count(&stack, "messages").await, 0);
    stack.finish().await;
}

#[tokio::test]
async fn newline_and_tab_in_message_are_accepted() {
    let stack = Stack::start(StackOpts::default()).await;
    let (resp, body) = send(
        &stack.app,
        post_json("/v1/turn", &turn("line one\n\tline two")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    stack.finish().await;
}

#[tokio::test]
async fn internal_errors_do_not_leak_database_text() {
    let stack = Stack::start(StackOpts::default()).await;
    sqlx::query("DROP TABLE messages CASCADE")
        .execute(&stack.db.pool)
        .await
        .expect("break the schema");
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hello"))).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "internal");
    assert_eq!(body["error"]["message"], "internal error");
    let shown = body.to_string().to_lowercase();
    for leak in ["messages", "relation", "sqlx", "database", "postgres"] {
        assert!(!shown.contains(leak), "{leak} leaked: {shown}");
    }
    assert!(resp.headers().contains_key("x-trace-id"));
    stack.finish().await;
}
