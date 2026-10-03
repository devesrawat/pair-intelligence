//! `POST /v1/policy/authorize` and `POST /v1/approvals`. Authorize decides; it never executes.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::stack::{Stack, StackOpts, APPROVER_TOKEN};
use common::{post_json, send, TOKEN};
use serde_json::{json, Value};

const SENTINEL: &str = "sentinel.txt";
/// The payload hash covers the task id, so an approval flow must reuse one.
const TASK: &str = "0199c0de-1234-7abc-8def-0123456789ab";

fn action(tool: &str) -> Value {
    json!({
        "tool": tool,
        "args": [],
        "paths": [],
        "data_class": "personal",
        "task_id": TASK,
    })
}

fn approval_request(body: &Value, approver: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/approvals")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-actor", "owner")
        .header("content-type", "application/json");
    if let Some(token) = approver {
        builder = builder.header("x-approver-token", token);
    }
    builder.body(Body::from(body.to_string())).expect("request")
}

#[tokio::test]
async fn deny_returns_deny_and_nothing_is_executed() {
    let stack = Stack::start(StackOpts::default()).await;
    let sentinel = stack.workspace.join(SENTINEL);
    std::fs::write(&sentinel, "keep me").expect("sentinel");

    // `rm` is not on the executable allowlist: denied, and the file it names must survive.
    let mut req = action("shell.exec");
    req["executable"] = json!("rm");
    req["args"] = json!(["-rf", sentinel.to_string_lossy()]);
    let (resp, body) = send(&stack.app, post_json("/v1/policy/authorize", &req)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(body["data"]["decision"]["decision"], "deny", "{body}");
    assert!(sentinel.exists(), "authorize must never execute anything");

    // Even an ALLOWED write is only a decision: the file is not created.
    let target = stack.workspace.join("created.txt");
    let mut write = action("fs.write");
    write["paths"] = json!([target.to_string_lossy()]);
    let (_, body) = send(&stack.app, post_json("/v1/policy/authorize", &write)).await;
    assert_eq!(body["data"]["decision"]["decision"], "allow", "{body}");
    assert!(!target.exists(), "allow is a verdict, not an execution");

    // A host credential path is denied.
    let mut ssh = action("fs.read");
    ssh["paths"] = json!(["~/.ssh/id_ed25519"]);
    let (_, body) = send(&stack.app, post_json("/v1/policy/authorize", &ssh)).await;
    assert_eq!(body["data"]["decision"]["decision"], "deny", "{body}");
    stack.finish().await;
}

#[tokio::test]
async fn needs_approval_returns_payload_hash() {
    let stack = Stack::start(StackOpts::default()).await;
    let (resp, body) = send(&stack.app, post_json("/v1/policy/authorize", &action("message.send"))).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(body["data"]["decision"]["decision"], "needs_approval", "{body}");
    let hash = body["data"]["decision"]["payload_hash"].as_str().expect("hash").to_owned();
    assert_eq!(hash.len(), 64, "sha256 hex");

    // The approver (a separate credential) approves exactly that hash.
    let (resp, body) = send(
        &stack.app,
        approval_request(&json!({"payload_hash": hash, "expires_in_secs": 3600}), Some(APPROVER_TOKEN)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    let approval_id = body["data"]["approval_id"].as_str().expect("approval id").to_owned();

    // Presenting the approval to authorize does not consume it: only a Gate path consumes.
    let mut again = action("message.send");
    again["approvals"] = json!([approval_id]);
    let (_, body) = send(&stack.app, post_json("/v1/policy/authorize", &again)).await;
    assert_eq!(body["data"]["decision"]["decision"], "needs_approval");
    assert_eq!(body["data"]["decision"]["payload_hash"], hash.as_str());
    let consumed: i64 = sqlx::query_scalar("SELECT count(*) FROM approvals WHERE consumed_at IS NOT NULL")
        .fetch_one(&stack.db.pool)
        .await
        .expect("count");
    assert_eq!(consumed, 0);
    stack.finish().await;
}

#[tokio::test]
async fn stale_policy_version_denied() {
    let stack = Stack::start(StackOpts::default()).await;
    let mut req = action("fs.read");
    req["policy_version"] = json!("1999-01-01.0");
    let (resp, body) = send(&stack.app, post_json("/v1/policy/authorize", &req)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(body["data"]["decision"]["decision"], "deny", "{body}");
    assert!(
        body["data"]["decision"]["reason"].as_str().is_some_and(|r| r.contains("version")),
        "{body}"
    );
    assert_eq!(body["data"]["policy_version"], stack.services.policy.version());
    stack.finish().await;
}

#[tokio::test]
async fn authorize_refuses_a_caller_chosen_workspace_root() {
    let stack = Stack::start(StackOpts::default()).await;
    let mut req = action("fs.read");
    req["workspace_root"] = json!("/");
    let (resp, body) = send(&stack.app, post_json("/v1/policy/authorize", &req)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_input");
    stack.finish().await;
}

#[tokio::test]
async fn approvals_need_the_separate_approver_credential() {
    let stack = Stack::start(StackOpts::default()).await;
    let hash = "a".repeat(64);
    let body = json!({"payload_hash": hash, "expires_in_secs": 60});
    for token in [None, Some("wrong-approver-token-0000")] {
        let (resp, out) = send(&stack.app, approval_request(&body, token)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{token:?}: {out}");
        assert_eq!(out["error"]["code"], "policy_denied");
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM approvals")
        .fetch_one(&stack.db.pool)
        .await
        .expect("count");
    assert_eq!(rows, 0, "the service token alone must not mint approvals");

    let too_long = json!({"payload_hash": hash, "expires_in_secs": 25 * 3600});
    let (resp, out) = send(&stack.app, approval_request(&too_long, Some(APPROVER_TOKEN))).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{out}");
    let bad_hash = json!({"payload_hash": "not-a-hash", "expires_in_secs": 60});
    let (resp, _) = send(&stack.app, approval_request(&bad_hash, Some(APPROVER_TOKEN))).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    stack.finish().await;
}
