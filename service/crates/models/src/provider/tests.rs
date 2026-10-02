//! Provider and persistence tests against local mock fixtures. No live network.
use super::mock::{self, Mock, Reply, TestDb};
use super::store::{ConversationStore, CostState, ModelCallRecord, NewMessage};
use super::{
    AnthropicProvider, Endpoint, Health, ModelEntry, OllamaCloudProvider, ProviderKind,
    ProviderRegistry,
};
use pair_core::error::ErrorCode;
use pair_core::ids::{ConversationId, TaskId, TraceId};
use pair_core::money::{Micros, Price};
use pair_core::traits::Provider;
use pair_core::types::{DataClass, ModelMessage, ModelRequest, TrustClass};
use pair_telemetry::Secret;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MODEL: &str = "claude-sonnet-5-5";
const RESOLVED: &str = "claude-sonnet-5-5-20260301";
const KEY: &str = "k_live_plainsecret99";

fn entry(id: &str, kind: ProviderKind, base: &str, priced: bool) -> ModelEntry {
    ModelEntry {
        id: id.to_owned(),
        provider: kind,
        endpoint: Endpoint::unchecked_for_tests(base),
        modalities: vec!["text".to_owned()],
        context_tokens: 200_000,
        max_output_tokens: 4096,
        tools: false,
        structured_output: false,
        price: priced.then(|| Price {
            version: "pv-test".to_owned(),
            input_per_mtok: Micros(2_000_000),
            output_per_mtok: Micros(10_000_000),
        }),
        data_policy: "test".to_owned(),
        allowed_data_classes: vec![DataClass::Public, DataClass::Personal],
        quota_requests_per_minute: None,
        health: Health::Healthy,
        id_verified: true,
    }
}

fn anthropic(base: &str) -> AnthropicProvider {
    let reg =
        ProviderRegistry::from_entries(vec![entry(MODEL, ProviderKind::Anthropic, base, true)])
            .expect("registry");
    AnthropicProvider::new(Secret::new(KEY), Arc::new(reg)).expect("provider")
}

fn request(model: &str, deadline_ms: u64) -> ModelRequest {
    ModelRequest {
        model_id: model.to_owned(),
        messages: vec![
            ModelMessage {
                role: "system".into(),
                content: "be brief".into(),
                trust: TrustClass::Owner,
            },
            ModelMessage {
                role: "user".into(),
                content: "hi".into(),
                trust: TrustClass::Owner,
            },
        ],
        max_output_tokens: 256,
        deadline_ms,
        data_class: DataClass::Public,
        task: TaskId::new(),
        trace: TraceId::new(),
    }
}

fn sse(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn happy_stream() -> Vec<String> {
    let start = sse(
        "message_start",
        &format!(
            r#"{{"type":"message_start","message":{{"id":"msg_1","model":"{RESOLVED}","usage":{{"input_tokens":25,"output_tokens":1}}}}}}"#
        ),
    );
    let d1 = sse(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello, "}}"#,
    );
    let d2 = sse(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
    );
    let delta = sse(
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
    );
    let stop = sse("message_stop", r#"{"type":"message_stop"}"#);
    let joined = format!(
        "{start}{sse_ping}{d1}{d2}{delta}{stop}",
        sse_ping = sse("ping", r#"{"type":"ping"}"#)
    );
    // Split at awkward byte boundaries to exercise the incremental parser.
    let mid = joined.len() / 3;
    let cut = joined
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| *i >= mid)
        .unwrap_or(mid);
    let (a, rest) = joined.split_at(cut);
    let cut2 = rest.len() / 2;
    let (b, c) = rest.split_at(cut2);
    vec![a.to_owned(), b.to_owned(), c.to_owned()]
}

async fn ok_mock() -> Mock {
    mock::start(Reply::Stream {
        status: 200,
        headers: vec![
            ("content-type", "text/event-stream".to_owned()),
            ("request-id", "req_abc".to_owned()),
        ],
        chunks: happy_stream(),
    })
    .await
}

#[tokio::test]
async fn streaming_completion_assembles_text_and_usage() {
    let m = ok_mock().await;
    let resp = anthropic(&m.base)
        .generate(request(MODEL, 5_000))
        .await
        .expect("generate");
    assert_eq!(resp.text, "Hello, world");
    assert_eq!(resp.usage.input_tokens, 25);
    assert_eq!(resp.usage.output_tokens, 15);
    // 25 * $2/M = 50 micros; 15 * $10/M = 150 micros.
    assert_eq!(resp.usage.actual_cost, Some(Micros(200)));
    assert_eq!(resp.usage.price_version, "pv-test");
    assert_eq!(resp.provider_request_id.as_deref(), Some("req_abc"));

    let cap = m.captured.lock().expect("lock");
    assert_eq!(cap.len(), 1);
    assert_eq!(cap[0].path, "/v1/messages");
    assert_eq!(cap[0].headers["x-api-key"], KEY);
    assert_eq!(cap[0].headers["anthropic-version"], "2023-06-01");
    assert_eq!(cap[0].body["stream"], true);
    assert_eq!(cap[0].body["system"], "be brief");
    assert_eq!(cap[0].body["messages"][0]["role"], "user");
    assert_eq!(cap[0].body["max_tokens"], 256);
}

async fn hang_mock() -> (Mock, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let m = mock::start(Reply::Hang {
        first: sse("ping", r#"{"type":"ping"}"#),
        dropped: dropped.clone(),
    })
    .await;
    (m, dropped)
}

async fn wait_for(flag: &AtomicBool) -> bool {
    for _ in 0..100 {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn cancellation_stops_request() {
    let (m, dropped) = hang_mock().await;
    let p = anthropic(&m.base);
    let task = tokio::spawn(async move { p.generate(request(MODEL, 60_000)).await });
    // Wait until the request reached the server, then cancel by dropping the future.
    for _ in 0..100 {
        if !m.captured.lock().expect("lock").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !dropped.load(Ordering::SeqCst),
        "request must still be in flight"
    );
    task.abort();
    assert!(
        wait_for(&dropped).await,
        "server never observed the client disconnect"
    );
}

#[tokio::test]
async fn provider_timeout_returns_error() {
    let (m, dropped) = hang_mock().await;
    let err = anthropic(&m.base)
        .generate(request(MODEL, 250))
        .await
        .expect_err("must time out");
    assert_eq!(err.code, ErrorCode::ProviderTimeout);
    assert!(
        wait_for(&dropped).await,
        "timeout must cancel the in-flight request"
    );
}

#[tokio::test]
async fn secrets_redacted_from_errors() {
    let body = format!(
        r#"{{"error":{{"message":"invalid x-api-key {KEY} and sk-ant-api03-ABCDEFGHIJ"}}}}"#
    );
    let m = mock::start(Reply::Stream {
        status: 401,
        headers: vec![],
        chunks: vec![body],
    })
    .await;
    let err = anthropic(&m.base)
        .generate(request(MODEL, 5_000))
        .await
        .expect_err("401");
    let text = format!("{err} {err:?}");
    assert!(!text.contains("plainsecret99"), "{text}");
    assert!(!text.contains("ABCDEFGHIJ"), "{text}");
    assert_eq!(err.code, ErrorCode::ProviderUnavailable);
    let p = anthropic(&m.base);
    assert!(!format!("{p:?}").contains("plainsecret99"));
}

#[tokio::test]
async fn unknown_price_blocks_paid_execution() {
    let reg = ProviderRegistry::from_entries(vec![entry(
        MODEL,
        ProviderKind::Anthropic,
        "http://127.0.0.1:1",
        false,
    )])
    .expect("reg");
    let p = AnthropicProvider::new(Secret::new(KEY), Arc::new(reg)).expect("provider");
    let err = p
        .generate(request(MODEL, 1_000))
        .await
        .expect_err("blocked");
    assert_eq!(err.code, ErrorCode::BudgetUnknownPrice);
}

fn unverified_anthropic(base: &str, allow: bool) -> AnthropicProvider {
    let mut e = entry(MODEL, ProviderKind::Anthropic, base, true);
    e.id_verified = false;
    let reg = ProviderRegistry::from_entries(vec![e]).expect("registry");
    AnthropicProvider::new(Secret::new(KEY), Arc::new(reg))
        .expect("provider")
        .with_allow_unverified_ids(allow)
}

#[tokio::test]
async fn unverified_model_id_refused_before_network() {
    let m = ok_mock().await;
    let err = unverified_anthropic(&m.base, false)
        .generate(request(MODEL, 5_000))
        .await
        .expect_err("refused");
    assert_eq!(err.code, ErrorCode::ProviderUnavailable);
    assert!(err.message.contains("PAIR_ALLOW_UNVERIFIED_MODEL_IDS"));
    assert!(m.captured.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn unverified_model_id_allowed_with_override() {
    let m = ok_mock().await;
    let resp = unverified_anthropic(&m.base, true)
        .generate(request(MODEL, 5_000))
        .await
        .expect("allowed");
    assert_eq!(resp.text, "Hello, world");
}

#[test]
fn registry_reports_unverified_ids_and_shipped_config_marks_guesses() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/models.yaml");
    let r = ProviderRegistry::load(&path).expect("models.yaml");
    let ids = r.unverified_ids();
    assert!(ids.contains(&"claude-sonnet-5-5".to_owned()), "{ids:?}");
    assert!(ids.contains(&"claude-haiku-4-5".to_owned()), "{ids:?}");
    assert!(r.get("gemma4:31b").expect("entry").id_verified);
}

#[tokio::test]
async fn disallowed_data_class_is_denied_before_network() {
    let p = anthropic("http://127.0.0.1:1");
    let mut req = request(MODEL, 1_000);
    req.data_class = DataClass::Employer;
    let err = p.generate(req).await.expect_err("denied");
    assert_eq!(err.code, ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn ollama_cloud_streams_ndjson_with_bearer_auth() {
    let lines = [
        r#"{"model":"gemma4:31b","message":{"role":"assistant","content":"po"},"done":false}"#,
        r#"{"model":"gemma4:31b","message":{"role":"assistant","content":"ng"},"done":false}"#,
        r#"{"model":"gemma4:31b","message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":7,"eval_count":2}"#,
    ];
    let chunks = lines.iter().map(|l| format!("{l}\n")).collect();
    let m = mock::start(Reply::Stream {
        status: 200,
        headers: vec![],
        chunks,
    })
    .await;
    let reg = ProviderRegistry::from_entries(vec![entry(
        "gemma4:31b",
        ProviderKind::OllamaCloud,
        &m.base,
        true,
    )])
    .expect("reg");
    let p = OllamaCloudProvider::new(Secret::new(KEY), Arc::new(reg)).expect("provider");
    let resp = p
        .generate(request("gemma4:31b", 5_000))
        .await
        .expect("generate");
    assert_eq!(resp.text, "pong");
    assert_eq!((resp.usage.input_tokens, resp.usage.output_tokens), (7, 2));
    let cap = m.captured.lock().expect("lock");
    assert_eq!(cap[0].path, "/api/chat");
    assert_eq!(cap[0].headers["authorization"], format!("Bearer {KEY}"));
    assert_eq!(cap[0].body["options"]["num_predict"], 256);
}

#[tokio::test]
async fn conversation_survives_reopen() {
    let db = TestDb::create().await;
    let trace = TraceId::new();
    let store = ConversationStore::new(db.pool().await);
    let conv = store
        .create_conversation("restart test", trace)
        .await
        .expect("create");
    let msg = |cid: &str, role: &str, text: &str| NewMessage {
        conversation: conv,
        client_message_id: cid.to_owned(),
        role: role.to_owned(),
        content: text.to_owned(),
        trust: TrustClass::Owner,
        trace,
    };
    let (m1, new1) = store
        .append_message(msg("c1", "user", "hello"))
        .await
        .expect("append");
    assert!(new1);
    store
        .append_message(msg("c2", "assistant", "hi there"))
        .await
        .expect("append");
    let (dup, new_dup) = store
        .append_message(msg("c1", "user", "CHANGED"))
        .await
        .expect("dup append");
    assert!(!new_dup);
    assert_eq!(dup.id, m1.id);
    assert_eq!(dup.content, "hello");
    store.pool().close().await;
    drop(store);

    let reopened = ConversationStore::new(db.pool().await);
    let msgs = reopened.list_messages(conv).await.expect("list");
    let seen: Vec<(&str, &str, i32)> = msgs
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str(), m.seq))
        .collect();
    assert_eq!(
        seen,
        vec![("user", "hello", 1), ("assistant", "hi there", 2)]
    );
    let missing = reopened
        .append_message(NewMessage {
            conversation: ConversationId::new(),
            ..msg("x", "user", "y")
        })
        .await
        .expect_err("unknown conversation");
    assert_eq!(missing.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn resolved_model_recorded() {
    let db = TestDb::create().await;
    let store = ConversationStore::new(db.pool().await);
    let m = ok_mock().await;
    let req = request(MODEL, 5_000);
    let resp = anthropic(&m.base)
        .generate(req.clone())
        .await
        .expect("generate");
    assert_eq!(resp.resolved_model, RESOLVED);

    let conv = store
        .create_conversation("calls", req.trace)
        .await
        .expect("conv");
    let rec =
        ModelCallRecord::from_response(&req, "anthropic", "baseline:routine", Some(conv), &resp);
    store.record_model_call(&rec).await.expect("record");
    let got = store.get_model_call(rec.id).await.expect("get");
    assert_eq!(got.requested_model, MODEL);
    assert_eq!(got.resolved_model.as_deref(), Some(RESOLVED));
    assert_eq!(got.cost, Some(Micros(200)));
    assert_eq!(got.price_version.as_deref(), Some("pv-test"));
    assert_eq!(got.cost_state, CostState::Priced);
    assert_eq!((got.input_tokens, got.output_tokens), (25, 15));
    assert_eq!(got.trace, req.trace);
    assert_eq!(got.route_reason, "baseline:routine");

    let reservation = pair_core::ids::ReservationId::new();
    store
        .mark_reconciled(rec.id, reservation)
        .await
        .expect("reconcile");
    let after = store.get_model_call(rec.id).await.expect("get");
    assert_eq!(after.cost_state, CostState::Reconciled);
    assert_eq!(after.reservation, Some(reservation));

    let audit = store
        .record_audit_event(
            req.trace,
            "system",
            "model_call",
            &rec.id.to_string(),
            &serde_json::json!({"ok": true}),
        )
        .await;
    assert!(audit.is_ok());
}
