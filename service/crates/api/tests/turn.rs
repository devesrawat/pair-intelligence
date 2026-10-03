//! End-to-end tests for `POST /v1/turn`: real Postgres, real budget, real conversation store,
//! loopback mock provider and Jev.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::mock::{JevMode, ProviderMode};
use common::stack::{budget_yaml, Stack, StackOpts, CHEAP, MID, PREMIUM, PRICE_VERSION};
use common::{post_json, send};
use pair_core::ids::{ConversationId, TaskId};
use pair_core::money::Micros;
use pair_core::types::{ReserveRequest, TaskKind};
use pair_models::provider::store::ConversationStore;
use serde_json::{json, Value};

const MID_COST_MICROS: i64 = 540; // 120 in x $2/M + 30 out x $10/M, rounded up per component

fn turn(message: &str, class: &str) -> Value {
    json!({ "message": message, "data_class": class })
}

async fn count(stack: &Stack, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&stack.db.pool)
        .await
        .expect("count query")
}

#[tokio::test]
async fn budget_denied_request_makes_zero_provider_hits() {
    let stack = Stack::start(StackOpts::default()).await;
    // Leave less than one worst-case reservation of the daily cap: the router (which only sees the
    // task cap) still admits a model, so it is the ledger that must refuse.
    stack
        .services
        .budget
        .reserve_with(ReserveRequest::metered(
            TaskId::new(),
            Micros(980_000),
            TaskKind::Coding,
            PRICE_VERSION.to_owned(),
        ))
        .await
        .expect("prefill the daily cap");

    let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hello", "public"))).await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(body["error"]["code"], "budget_exceeded");
    assert_eq!(
        stack.provider.hits(),
        0,
        "a denied task must never reach the provider"
    );
    assert_eq!(count(&stack, "SELECT count(*) FROM model_calls").await, 0);
    stack.finish().await;
}

#[tokio::test]
async fn turn_persists_and_survives_pool_reopen_with_reconciled_cost() {
    let stack = Stack::start(StackOpts::default()).await;
    let (resp, body) = send(
        &stack.app,
        post_json("/v1/turn", &turn("hello there", "personal")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    let data = &body["data"];
    assert_eq!(data["text"], "hello from the mock");
    assert_eq!(data["resolved_model"], format!("{MID}-resolved"));
    assert_eq!(data["cost_state"], "reconciled");
    assert!(data["route_reason"].as_str().is_some_and(|r| !r.is_empty()));
    assert_eq!(
        data["trace_id"],
        resp.headers()["x-trace-id"].to_str().expect("ascii")
    );
    let conversation = ConversationId(
        data["conversation_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .expect("conversation id"),
    );

    // A restarted service opens a fresh pool: everything must still be there.
    let reopened = stack.db.reopen_pool().await;
    let store = ConversationStore::new(reopened.clone());
    let messages = store.list_messages(conversation).await.expect("list");
    let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant"]);
    assert_eq!(messages[1].content, "hello from the mock");

    let (cost_state, cost, reservation_state, counted): (String, i64, String, i64) =
        sqlx::query_as(
            "SELECT c.cost_state, c.cost_micros, r.state, r.counted_micros \
         FROM model_calls c JOIN budget_reservations r ON r.id = c.reservation_id",
        )
        .fetch_one(&reopened)
        .await
        .expect("model call joined to its reservation");
    assert_eq!(cost_state, "reconciled");
    assert_eq!(cost, MID_COST_MICROS);
    assert_eq!(reservation_state, "settled");
    assert_eq!(counted, MID_COST_MICROS);

    // The conversation continues across the restart.
    let mut next = turn("and again", "personal");
    next["conversation_id"] = data["conversation_id"].clone();
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &next)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(
        store.list_messages(conversation).await.expect("list").len(),
        4
    );
    reopened.close().await;
    stack.finish().await;
}

#[tokio::test]
async fn employer_data_refused_before_any_network_call() {
    let stack = Stack::start(StackOpts {
        jev: Some(JevMode::Ok),
        ..StackOpts::default()
    })
    .await;
    for (class, code) in [
        ("employer", "provider_disallowed"),
        ("sensitive", "provider_disallowed"),
        ("top-secret", "policy_denied"),
    ] {
        let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("payroll", class))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{class}: {body}");
        assert_eq!(body["error"]["code"], code, "{class}");
    }
    assert_eq!(stack.provider.hits(), 0);
    let jev_hits = stack.jev.as_ref().map_or(usize::MAX, |j| j.hits());
    assert_eq!(
        jev_hits, 0,
        "the classifier must not see refused data either"
    );
    for table in [
        "conversations",
        "messages",
        "model_calls",
        "budget_reservations",
        "audit_events",
    ] {
        let n = count(&stack, &format!("SELECT count(*) FROM {table}")).await;
        assert_eq!(n, 0, "{table} must stay empty");
    }
    stack.finish().await;
}

#[tokio::test]
async fn jev_failure_falls_back_to_baseline_and_still_answers() {
    let stack = Stack::start(StackOpts {
        jev: Some(JevMode::Fail(503)),
        allow_turn_kind_override: true,
        ..StackOpts::default()
    })
    .await;
    // `coding` kind on purpose: the classifier's own reservation must not pin the task to `default`.
    let mut body = turn("fix the failing test", "public");
    body["kind"] = json!("coding");
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(body["data"]["resolved_model"], format!("{MID}-resolved"));
    assert!(
        body["data"]["route_reason"]
            .as_str()
            .is_some_and(|r| r.contains("baseline used")),
        "{body}"
    );
    assert_eq!(
        stack.jev.as_ref().map_or(0, |j| j.hits()),
        1,
        "no classifier retry"
    );
    // The failed classifier call is an unknown charge: unresolved, never zero.
    let classifier_state: String =
        sqlx::query_scalar("SELECT state FROM budget_reservations WHERE category = 'classifier'")
            .fetch_one(&stack.db.pool)
            .await
            .expect("classifier reservation");
    assert_eq!(classifier_state, "unresolved");
    stack.finish().await;
}

#[tokio::test]
async fn jev_shadow_mode_records_the_recommendation_and_does_not_change_the_route() {
    let stack = Stack::start(StackOpts {
        jev: Some(JevMode::Ok),
        allow_turn_kind_override: true,
        ..StackOpts::default()
    })
    .await;
    let mut body = turn("fix the failing test", "public");
    body["kind"] = json!("research");
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["resolved_model"],
        format!("{MID}-resolved"),
        "baseline still runs"
    );
    let detail: Value =
        sqlx::query_scalar("SELECT detail FROM audit_events WHERE kind = 'routing_decision'")
            .fetch_one(&stack.db.pool)
            .await
            .expect("routing decision audit event");
    assert_eq!(detail["classification"]["intent"], "coding");
    assert_eq!(detail["recommendation"]["applied"], false);
    stack.finish().await;
}

#[tokio::test]
async fn unverified_model_id_stops_the_call_and_does_not_escalate() {
    let stack = Stack::start(StackOpts {
        unverified: vec![CHEAP],
        baseline_tier: "routine",
        ..StackOpts::default()
    })
    .await;
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hello", "public"))).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "provider_disallowed");
    assert_eq!(
        stack.provider.hits_for(PREMIUM),
        0,
        "must not escalate to the premium model"
    );
    assert_eq!(stack.provider.hits_for(MID), 0);
    assert_eq!(
        stack.provider.hits(),
        0,
        "the unverified model itself is refused up front"
    );
    assert_eq!(
        count(&stack, "SELECT count(*) FROM budget_reservations").await,
        0,
        "a refusal that was certain in advance must not strand a reservation"
    );
    stack.finish().await;
}

#[tokio::test]
async fn provider_failure_leaves_reservation_unresolved() {
    let stack = Stack::start(StackOpts {
        provider: ProviderMode::Fail(500),
        ..StackOpts::default()
    })
    .await;
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hello", "public"))).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "provider_unavailable");
    // The first attempt's unresolved worst-case reservation uses up the $0.10 task cap, so the
    // router's next model is refused by the ledger before it reaches the provider.
    let hits = i64::try_from(stack.provider.hits()).expect("small");
    assert_eq!(
        hits, 1,
        "one provider call, then the task cap stops the escalation"
    );
    assert_eq!(
        count(
            &stack,
            "SELECT count(*) FROM budget_reservations WHERE state = 'unresolved'"
        )
        .await,
        hits,
        "every failed attempt keeps its full reservation counted"
    );
    assert_eq!(
        count(
            &stack,
            "SELECT count(*) FROM budget_reservations WHERE state <> 'unresolved'"
        )
        .await,
        0,
        "nothing may be settled at zero"
    );
    assert_eq!(
        count(
            &stack,
            "SELECT count(*) FROM model_calls WHERE status = 'error'"
        )
        .await,
        hits,
        "every provider call is recorded, failures included"
    );
    stack.finish().await;
}

#[tokio::test]
async fn turn_requires_service_token() {
    let stack = Stack::start(StackOpts::default()).await;
    let no_auth = Request::builder()
        .method("POST")
        .uri("/v1/turn")
        .header("content-type", "application/json")
        .body(Body::from(turn("hello", "public").to_string()))
        .expect("request");
    let (resp, body) = send(&stack.app, no_auth).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{body}");
    let wrong = Request::builder()
        .method("POST")
        .uri("/v1/turn")
        .header("authorization", "Bearer wrong-token-wrong-token")
        .header("x-actor", "tester")
        .header("content-type", "application/json")
        .body(Body::from(turn("hello", "public").to_string()))
        .expect("request");
    assert_eq!(
        send(&stack.app, wrong).await.0.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(stack.provider.hits(), 0);
    stack.finish().await;
}

#[tokio::test]
async fn caps_to_zero_refuses_new_turns() {
    let stack = Stack::start(StackOpts {
        budget_yaml: budget_yaml("0.00", "0.00", "0.00", "0.00"),
        ..StackOpts::default()
    })
    .await;
    let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hello", "public"))).await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "{body}");
    assert_eq!(body["error"]["code"], "budget_exceeded");
    assert_eq!(stack.provider.hits(), 0);
    stack.finish().await;
}

mod subscription {
    use super::*;
    use common::stack::{SubscriptionOpts, SUB, SUB_TEXT};

    async fn rows(stack: &Stack) -> Vec<(String, String, Option<i64>, String)> {
        sqlx::query_as(
            "SELECT c.requested_model, c.cost_state, c.cost_micros, r.state \
             FROM model_calls c JOIN budget_reservations r ON r.id = c.reservation_id \
             ORDER BY c.created_at",
        )
        .fetch_all(&stack.db.pool)
        .await
        .expect("model calls")
    }

    #[tokio::test]
    async fn test_turn_subscription_primary_answers_records_and_settles_at_zero() {
        let stack = Stack::start(StackOpts {
            subscription: Some(SubscriptionOpts::Primary),
            ..StackOpts::default()
        })
        .await;
        let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hi", "personal"))).await;
        assert_eq!(resp.status(), StatusCode::OK, "{body}");
        assert_eq!(body["data"]["text"], SUB_TEXT);
        assert_eq!(body["data"]["resolved_model"], "claude-sonnet-5-5-test");
        assert_eq!(body["data"]["cost_state"], "reconciled");
        assert_eq!(stack.provider.hits(), 0, "no metered call may be made");
        let rows = rows(&stack).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0],
            (SUB.into(), "reconciled".into(), Some(0), "settled".into())
        );
        stack.finish().await;
    }

    #[tokio::test]
    async fn test_turn_metered_failure_falls_through_to_subscription_with_both_attempts_recorded() {
        let stack = Stack::start(StackOpts {
            subscription: Some(SubscriptionOpts::Fallback),
            provider: ProviderMode::Fail(503),
            ..StackOpts::default()
        })
        .await;
        let (resp, body) = send(&stack.app, post_json("/v1/turn", &turn("hi", "personal"))).await;
        assert_eq!(resp.status(), StatusCode::OK, "{body}");
        assert_eq!(body["data"]["text"], SUB_TEXT);
        let rows = rows(&stack).await;
        assert_eq!(
            rows.len(),
            2,
            "failed metered attempt and the subscription success: {rows:?}"
        );
        assert_eq!(
            rows[0].0, MID,
            "the failure stays attributed to the metered model"
        );
        assert_ne!(rows[0].1, "reconciled");
        assert_eq!(
            rows[0].2, None,
            "a failed call's cost stays unknown, never assumed zero"
        );
        assert_eq!(
            rows[1],
            (SUB.into(), "reconciled".into(), Some(0), "settled".into())
        );
        stack.finish().await;
    }
}
