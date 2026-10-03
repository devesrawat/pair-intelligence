//! Spec section 6: at most 3 model attempts per task in total. The counter is persisted, so step
//! retries and restarts cannot multiply it.

mod common;

use axum::http::StatusCode;
use common::mock::ProviderMode;
use common::stack::{Stack, StackOpts};
use common::{post_json, send};
use pair_api::attempts::AttemptStore;
use pair_api::turn::recording::{TracedBudget, TracedProvider, TurnTrace};
use pair_api::turn::request::ValidTurn;
use pair_core::error::ErrorCode;
use pair_core::ids::TaskId;
use pair_core::types::{DataClass, ModelMessage, ModelRequest, TaskKind, TrustClass};
use pair_workflows::calls::ModelCaller;
use pair_workflows::limits::RunLimits;
use serde_json::json;

const MAX_ATTEMPTS: u32 = 3;

fn request(task: TaskId) -> ModelRequest {
    ModelRequest {
        model_id: String::new(),
        messages: vec![ModelMessage {
            role: "user".into(),
            content: "retry me".into(),
            trust: TrustClass::Owner,
        }],
        max_output_tokens: 256,
        deadline_ms: 5_000,
        data_class: DataClass::Public,
        task,
        trace: pair_core::ids::TraceId::new(),
    }
}

#[tokio::test]
async fn attempt_cap_is_persisted_across_step_retries() {
    let stack = Stack::start(StackOpts {
        provider: ProviderMode::Fail(500),
        ..StackOpts::default()
    })
    .await;
    let svc = &stack.services;
    let task = TaskId::new();
    let trace = TurnTrace::default();
    let budget = TracedBudget {
        inner: &svc.budget,
        attempts: &svc.attempts,
        trace: trace.clone(),
    };
    let provider = TracedProvider {
        inner: svc.provider.as_ref(),
        trace,
    };
    let limits = RunLimits::interactive();
    let caller = ModelCaller {
        provider: &provider,
        budget: &budget,
        prices: svc.registry.as_ref(),
        planner: svc.pipeline.router(),
        limits: &limits,
        // Coding cap ($1.00): the task budget is not what stops this task, the attempt cap is.
        kind: TaskKind::Coding,
        intent: "coding",
    };

    // Step run 1: the router offers three models; each fails and each took an attempt.
    let first = caller
        .generate(request(task))
        .await
        .expect_err("provider is down");
    assert_eq!(first.code, ErrorCode::ProviderUnavailable);
    assert_eq!(stack.provider.hits(), 3);
    assert_eq!(svc.attempts.used(task).await.expect("used"), MAX_ATTEMPTS);

    // Step retries 2 and 3: each would get a fresh per-call limiter of 3 without persistence.
    for retry in 2..=3 {
        let err = caller.generate(request(task)).await.expect_err("cap spent");
        assert_eq!(err.code, ErrorCode::BudgetExceeded, "retry {retry}: {err}");
        assert_eq!(
            stack.provider.hits(),
            3,
            "retry {retry} must not reach the provider"
        );
    }
    stack.finish().await;
}

#[tokio::test]
async fn fourth_attempt_for_same_task_refused_even_after_restart() {
    let stack = Stack::start(StackOpts {
        provider: ProviderMode::Fail(500),
        ..StackOpts::default()
    })
    .await;
    let conversation = stack
        .services
        .store
        .create_conversation_with_class("retry", pair_core::ids::TraceId::new(), DataClass::Public)
        .await
        .expect("conversation");
    let body = json!({
        "conversation_id": conversation.0,
        "client_message_id": "turn-1",
        "message": "hello",
        "data_class": "public",
        "kind": "coding",
    });
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &body)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{out}");
    assert_eq!(stack.provider.hits(), 3, "three attempts, all failed");

    // The same logical turn again (a client retry): refused before any provider call.
    let (resp, out) = send(&stack.app, post_json("/v1/turn", &body)).await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "{out}");
    assert_eq!(stack.provider.hits(), 3);

    // A restarted service opens a new pool: the counter is still spent.
    let task = ValidTurn {
        conversation: Some(conversation),
        client_message_id: Some("turn-1".into()),
        message: "hello".into(),
        data_class: DataClass::Public,
        kind: TaskKind::Coding,
        project_type: None,
    }
    .task_id();
    let reopened = stack.db.reopen_pool().await;
    let after_restart = AttemptStore::new(reopened.clone(), MAX_ATTEMPTS);
    assert_eq!(after_restart.used(task).await.expect("used"), MAX_ATTEMPTS);
    let err = after_restart
        .consume(task)
        .await
        .expect_err("fourth attempt");
    assert_eq!(err.code, ErrorCode::BudgetExceeded);

    // A different task is unaffected.
    assert_eq!(
        after_restart
            .consume(TaskId::new())
            .await
            .expect("fresh task"),
        1
    );
    reopened.close().await;
    stack.finish().await;
}

#[tokio::test]
async fn concurrent_consumers_cannot_exceed_the_attempt_cap() {
    let stack = Stack::start(StackOpts::default()).await;
    let store = AttemptStore::new(stack.db.pool.clone(), MAX_ATTEMPTS);
    let task = TaskId::new();
    let mut joins = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        joins.push(tokio::spawn(
            async move { store.consume(task).await.is_ok() },
        ));
    }
    let mut granted = 0;
    for j in joins {
        if j.await.expect("join") {
            granted += 1;
        }
    }
    assert_eq!(granted, MAX_ATTEMPTS, "exactly the cap, never more");
    stack.finish().await;
}
