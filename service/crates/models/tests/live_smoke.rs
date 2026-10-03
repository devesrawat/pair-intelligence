//! Bounded live smoke test against the real Anthropic API. Ignored by default.
//!
//! Run: `ANTHROPIC_API_KEY=... cargo test -p pair-models --test live_smoke -- --ignored --nocapture`
//! Cost bound: one request, <= 16 output tokens, Haiku pricing (well under $0.001).
use pair_core::ids::{TaskId, TraceId};
use pair_core::traits::Provider;
use pair_core::types::{DataClass, ModelMessage, ModelRequest, TrustClass};
use pair_models::provider::{
    AnthropicProvider, ClaudeCodeProvider, ProviderKind, ProviderRegistry,
};
use std::path::Path;
use std::sync::Arc;

const SMOKE_MODEL: &str = "claude-haiku-4-5";
const SMOKE_MAX_OUTPUT_TOKENS: u32 = 16;
const SMOKE_DEADLINE_MS: u64 = 30_000;

#[tokio::test]
#[ignore = "live network call; needs ANTHROPIC_API_KEY"]
async fn live_anthropic_smoke() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/models.yaml");
    let registry = Arc::new(ProviderRegistry::load(&path).expect("models.yaml"));
    let provider = AnthropicProvider::from_env(registry)
        .expect("ANTHROPIC_API_KEY must be set")
        // Unverified ids are exactly what live_anthropic_model_ids_exist checks.
        .with_allow_unverified_ids(true);
    let req = ModelRequest {
        model_id: SMOKE_MODEL.to_owned(),
        messages: vec![ModelMessage {
            role: "user".into(),
            content: "Reply with the single word: pong".into(),
            trust: TrustClass::Owner,
        }],
        max_output_tokens: SMOKE_MAX_OUTPUT_TOKENS,
        deadline_ms: SMOKE_DEADLINE_MS,
        data_class: DataClass::Public,
        task: TaskId::new(),
        trace: TraceId::new(),
    };
    let resp = provider.generate(req).await.expect("live call");
    println!(
        "resolved_model={} text={:?} usage={:?}",
        resp.resolved_model, resp.text, resp.usage
    );
    assert!(!resp.text.is_empty());
    assert!(resp.usage.input_tokens > 0 && resp.usage.output_tokens > 0);
    assert!(resp.usage.actual_cost.is_some());
}

/// One real call through the `claude` CLI on this host, using its subscription login.
/// Run: `cargo test -p pair-models --test live_smoke live_claude_code_smoke -- --ignored --nocapture`
#[tokio::test]
#[ignore = "live call; needs the claude CLI logged in with a subscription"]
async fn live_claude_code_smoke() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/models.yaml");
    let registry = Arc::new(ProviderRegistry::load(&path).expect("models.yaml"));
    let provider = ClaudeCodeProvider::from_env(registry);
    let req = ModelRequest {
        model_id: "claude-code/haiku".to_owned(),
        messages: vec![ModelMessage {
            role: "user".into(),
            content: "Reply with the single word: pong".into(),
            trust: TrustClass::Owner,
        }],
        max_output_tokens: SMOKE_MAX_OUTPUT_TOKENS,
        deadline_ms: SMOKE_DEADLINE_MS,
        data_class: DataClass::Public,
        task: TaskId::new(),
        trace: TraceId::new(),
    };
    let resp = provider.generate(req).await.expect("live call");
    println!(
        "resolved_model={} text={:?} usage={:?}",
        resp.resolved_model, resp.text, resp.usage
    );
    assert!(resp.text.to_lowercase().contains("pong"));
    assert!(resp.resolved_model.contains("haiku"));
    assert_eq!(resp.usage.price_version, "subscription");
}

const MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=1000";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Confirms every Anthropic id in config/models.yaml exists in the live catalog.
/// On success, drop `id_verified: false` from the YAML entries.
#[tokio::test]
#[ignore = "live network call; needs ANTHROPIC_API_KEY"]
async fn live_anthropic_model_ids_exist() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/models.yaml");
    let registry = ProviderRegistry::load(&path).expect("models.yaml");
    let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set");
    let body: serde_json::Value = reqwest::Client::new()
        .get(MODELS_URL)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .send()
        .await
        .expect("GET /v1/models")
        .error_for_status()
        .expect("models status")
        .json()
        .await
        .expect("models json");
    let live: Vec<&str> = body["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    let missing: Vec<&str> = registry
        .iter()
        .filter(|e| e.provider == ProviderKind::Anthropic)
        .map(|e| e.id.as_str())
        .filter(|id| !live.contains(id))
        .collect();
    assert!(
        missing.is_empty(),
        "ids not in live catalog: {missing:?}; live: {live:?}"
    );
}
