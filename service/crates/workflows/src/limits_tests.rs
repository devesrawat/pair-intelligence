#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::{
    calls::ModelCaller,
    coding::{
        testkit::{
            fake_ctx, host_sandbox, FakeBudget, FakePolicy, FixedPlanner, FixedPrices, FnProvider,
        },
        Runner, RunnerSetup,
    },
    limits::{RunLimits, MAX_TOOL_CALLS},
};
use pair_core::{
    error::{ErrorCode, PairError},
    ids::{TaskId, TraceId},
    types::{DataClass, ModelMessage, ModelRequest, TaskKind, TrustClass},
};
use pair_policy::Gate;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn runner<'a>(gate: &'a Gate, dir: &std::path::Path, limits: Arc<RunLimits>) -> Runner<'a> {
    Runner::new(
        gate,
        RunnerSetup {
            task: TaskId::new(),
            trace: TraceId::new(),
            ctx: fake_ctx(dir),
            home: dir.join("home"),
            timeout: Duration::from_secs(60),
            passthrough: Vec::new(),
            data_class: DataClass::Personal,
        },
    )
    .with_sandbox(host_sandbox())
    .with_limits(limits)
}

fn temp() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pair_t_lim_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(ToString::to_string).collect()
}

fn request(class: DataClass) -> ModelRequest {
    ModelRequest {
        model_id: String::new(),
        messages: vec![ModelMessage {
            role: "user".into(),
            content: "hi".into(),
            trust: TrustClass::Owner,
        }],
        max_output_tokens: 10,
        deadline_ms: 120_000,
        data_class: class,
        task: TaskId::new(),
        trace: TraceId::new(),
    }
}

#[tokio::test]
async fn tool_call_cap_20_enforced() {
    let dir = temp();
    let gate = Gate::new(Arc::new(FakePolicy::default()), None);
    let limits = Arc::new(RunLimits::interactive());
    let r = runner(&gate, &dir, limits.clone());
    for i in 0..MAX_TOOL_CALLS {
        let rep = r
            .run(&argv(&["true"]), &dir)
            .await
            .unwrap_or_else(|e| panic!("call {i}: {e}"));
        assert!(rep.passed());
    }
    let err = r
        .run(&argv(&["sh", "-c", "touch ran.marker"]), &dir)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);
    assert!(
        !dir.join("ran.marker").exists(),
        "the 21st command must not run"
    );
    assert_eq!(limits.tool_calls_used(), MAX_TOOL_CALLS);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn interactive_deadline_enforced() {
    let dir = temp();
    let gate = Gate::new(Arc::new(FakePolicy::default()), None);

    // already past the deadline: no command and no model call is started
    let expired = Arc::new(RunLimits::with_deadline(
        MAX_TOOL_CALLS,
        Instant::now() + Duration::from_millis(20),
    ));
    tokio::time::sleep(Duration::from_millis(60)).await;
    let err = runner(&gate, &dir, expired.clone())
        .run(&argv(&["sh", "-c", "touch ran.marker"]), &dir)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);
    assert!(!dir.join("ran.marker").exists());
    let provider = FnProvider::scripted(vec!["x".into()]);
    let budget = FakeBudget::default();
    let planner = FixedPlanner::single();
    let caller = ModelCaller {
        provider: &provider,
        budget: &budget,
        prices: &FixedPrices::standard(),
        planner: &planner,
        limits: &expired,
        kind: TaskKind::Coding,
        intent: "coding",
    };
    let err = caller
        .generate(request(DataClass::Personal))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);
    assert_eq!(provider.call_count(), 0);

    // a running command is cut off at the deadline, not at its own (longer) timeout
    let soon = Arc::new(RunLimits::with_deadline(
        MAX_TOOL_CALLS,
        Instant::now() + Duration::from_millis(800),
    ));
    let started = Instant::now();
    let rep = runner(&gate, &dir, soon)
        .run(&argv(&["sleep", "30"]), &dir)
        .await
        .unwrap();
    assert!(rep.timed_out);
    assert!(started.elapsed() < Duration::from_secs(10));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn model_call_deadline_is_bounded_by_the_run_deadline() {
    let provider = FnProvider::scripted(vec!["x".into()]);
    let budget = FakeBudget::default();
    let planner = FixedPlanner::single();
    let limits = RunLimits::with_deadline(MAX_TOOL_CALLS, Instant::now() + Duration::from_secs(5));
    let caller = ModelCaller {
        provider: &provider,
        budget: &budget,
        prices: &FixedPrices::standard(),
        planner: &planner,
        limits: &limits,
        kind: TaskKind::Coding,
        intent: "coding",
    };
    caller.generate(request(DataClass::Personal)).await.unwrap();
    let seen = provider.seen.lock().unwrap();
    assert!(seen[0].deadline_ms <= 5_000, "{}", seen[0].deadline_ms);
}

fn caller<'a>(
    provider: &'a FnProvider,
    budget: &'a FakeBudget,
    prices: &'a FixedPrices,
    planner: &'a FixedPlanner,
    limits: &'a RunLimits,
) -> ModelCaller<'a> {
    ModelCaller {
        provider,
        budget,
        prices,
        planner,
        limits,
        kind: TaskKind::Research,
        intent: "research",
    }
}

#[tokio::test]
async fn attempt_limit_3_enforced() {
    let provider = FnProvider::new(Box::new(|_, _| {
        Err(PairError::new(ErrorCode::ProviderUnavailable, "down"))
    }));
    let budget = FakeBudget::default();
    let planner = FixedPlanner::new(&["m1", "m2", "m3", "m4", "m5"]);
    let limits = RunLimits::interactive();
    let prices = FixedPrices::standard();
    let err = caller(&provider, &budget, &prices, &planner, &limits)
        .generate(request(DataClass::Public))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BudgetExceeded, "{}", err.message);
    assert_eq!(provider.call_count(), 3);
    let tried: Vec<String> = provider
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.model_id.clone())
        .collect();
    assert!(tried.is_empty() || tried == ["m1", "m2", "m3"]);
}

#[tokio::test]
async fn router_plan_drives_model_choice_and_only_unavailability_falls_back() {
    // first model unavailable -> second model answers; the router saw the data class
    let provider = FnProvider::new(Box::new(|i, _| {
        if i == 0 {
            Err(PairError::new(ErrorCode::ProviderTimeout, "slow"))
        } else {
            Ok("answer".into())
        }
    }));
    let budget = FakeBudget::default();
    let planner = FixedPlanner::new(&["primary", "backup"]);
    let limits = RunLimits::interactive();
    let prices = FixedPrices::standard();
    let resp = caller(&provider, &budget, &prices, &planner, &limits)
        .generate(request(DataClass::Sensitive))
        .await
        .unwrap();
    assert_eq!(resp.text, "answer");
    {
        let seen = provider.seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "only the successful call is recorded by the fake"
        );
        assert_eq!(seen[0].model_id, "backup");
        let profiles = planner.profiles.lock().unwrap();
        assert_eq!(profiles[0].data_class, DataClass::Sensitive);
        assert_eq!(profiles[0].intent, "research");
    }

    // a non-availability error stops at once
    let provider = FnProvider::new(Box::new(|_, _| {
        Err(PairError::new(ErrorCode::InvalidInput, "bad request"))
    }));
    let err = caller(&provider, &budget, &prices, &planner, &limits)
        .generate(request(DataClass::Public))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    assert_eq!(provider.call_count(), 1);
}

#[test]
fn caller_cannot_raise_tool_call_or_deadline_caps() {
    use crate::limits::{BACKGROUND_WALL, INTERACTIVE_WALL};
    let greedy = RunLimits::with_deadline(1000, Instant::now() + Duration::from_secs(10 * 60 * 60));
    for _ in 0..MAX_TOOL_CALLS {
        greedy.begin_tool_call().unwrap();
    }
    assert_eq!(
        greedy.begin_tool_call().unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    assert!(greedy.remaining().unwrap() <= BACKGROUND_WALL);
    // background limits are refused where the interactive wall applies
    assert!(RunLimits::background()
        .require_within(INTERACTIVE_WALL)
        .is_err());
    assert!(RunLimits::interactive()
        .require_within(INTERACTIVE_WALL)
        .is_ok());
    assert!(RunLimits::interactive()
        .require_within(BACKGROUND_WALL)
        .is_ok());
}
