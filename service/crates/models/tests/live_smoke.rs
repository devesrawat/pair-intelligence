//! Bounded live smoke test against the real Anthropic API. Ignored by default.
//!
//! Run: `ANTHROPIC_API_KEY=... cargo test -p pair-models --test live_smoke -- --ignored --nocapture`
//! Cost bound: one request, <= 16 output tokens, Haiku pricing (well under $0.001).
use pair_core::ids::{TaskId, TraceId};
use pair_core::traits::Provider;
use pair_core::types::{DataClass, ModelMessage, ModelRequest, TrustClass};
use pair_models::provider::{AnthropicProvider, ProviderRegistry};
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
    let provider = AnthropicProvider::from_env(registry).expect("ANTHROPIC_API_KEY must be set");
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
    println!("resolved_model={} text={:?} usage={:?}", resp.resolved_model, resp.text, resp.usage);
    assert!(!resp.text.is_empty());
    assert!(resp.usage.input_tokens > 0 && resp.usage.output_tokens > 0);
    assert!(resp.usage.actual_cost.is_some());
}
