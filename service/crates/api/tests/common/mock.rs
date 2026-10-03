//! Loopback mock servers: an Anthropic-style SSE provider and a Jev classifier. Both count hits.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub enum ProviderMode {
    /// 200 with an SSE stream carrying this text and these token counts.
    Ok {
        text: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Non-2xx response: the adapter maps it to `ProviderUnavailable`.
    Fail(u16),
}

impl ProviderMode {
    pub fn ok(text: &str) -> Self {
        Self::Ok {
            text: text.to_owned(),
            input_tokens: 120,
            output_tokens: 30,
        }
    }
}

#[derive(Clone)]
pub struct Mock {
    pub base: String,
    models: Arc<Mutex<Vec<String>>>,
    hits: Arc<Mutex<usize>>,
}

impl Mock {
    pub fn hits(&self) -> usize {
        *self.hits.lock().expect("hits lock")
    }

    /// Hits that targeted this model id (provider mock only).
    pub fn hits_for(&self, model: &str) -> usize {
        self.models
            .lock()
            .expect("models lock")
            .iter()
            .filter(|m| m.as_str() == model)
            .count()
    }
}

#[derive(Clone)]
struct ProviderState {
    mode: ProviderMode,
    mock: Mock,
}

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn stream_body(model: &str, text: &str, input: u64, output: u64) -> String {
    let start = json!({"type":"message_start","message":{"id":"msg_mock","model":format!("{model}-resolved"),"usage":{"input_tokens":input,"output_tokens":1}}});
    let delta = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}});
    let end = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":output}});
    format!(
        "{}{}{}{}",
        sse("message_start", &start),
        sse("content_block_delta", &delta),
        sse("message_delta", &end),
        sse("message_stop", &json!({"type":"message_stop"}))
    )
}

async fn provider_handler(State(st): State<ProviderState>, body: String) -> Response {
    *st.mock.hits.lock().expect("hits lock") += 1;
    let model = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["model"].as_str().map(str::to_owned))
        .unwrap_or_default();
    st.mock
        .models
        .lock()
        .expect("models lock")
        .push(model.clone());
    match st.mode {
        ProviderMode::Ok {
            text,
            input_tokens,
            output_tokens,
        } => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            stream_body(&model, &text, input_tokens, output_tokens),
        )
            .into_response(),
        ProviderMode::Fail(code) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            "{\"error\":\"mock failure\"}",
        )
            .into_response(),
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn empty_mock(base: String) -> Mock {
    Mock {
        base,
        models: Arc::new(Mutex::new(Vec::new())),
        hits: Arc::new(Mutex::new(0)),
    }
}

pub async fn spawn_provider(mode: ProviderMode) -> Mock {
    let mock = empty_mock(String::new());
    let app = Router::new()
        .route("/v1/messages", post(provider_handler))
        .with_state(ProviderState {
            mode,
            mock: mock.clone(),
        });
    let base = serve(app).await;
    Mock { base, ..mock }
}

#[derive(Clone)]
pub enum JevMode {
    Ok,
    Fail(u16),
}

#[derive(Clone)]
struct JevState {
    mode: JevMode,
    hits: Arc<Mutex<usize>>,
}

fn dist(labels: &[&str], chosen: &str, top: f64) -> Value {
    let rest = (1.0 - top) / (labels.len() as f64 - 1.0);
    let m: serde_json::Map<String, Value> = labels
        .iter()
        .map(|l| ((*l).to_owned(), json!(if *l == chosen { top } else { rest })))
        .collect();
    Value::Object(m)
}

fn jev_ok_body() -> Value {
    let intents = [
        "coding",
        "research",
        "planning",
        "memory_recall",
        "transformation",
        "mixed",
        "uncertain",
    ];
    let difficulties = ["routine", "substantial", "deep", "uncertain"];
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "intent": {"type": "choice", "choice": "coding", "confidence": 0.95,
                "probabilities": dist(&intents, "coding", 0.95)},
            "difficulty": {"type": "choice", "choice": "routine", "confidence": 0.95,
                "probabilities": dist(&difficulties, "routine", 0.95)}
        },
        "usage": {"input_tokens": 300, "output_tokens": 20}
    })
}

async fn jev_handler(State(st): State<JevState>, _body: String) -> Response {
    *st.hits.lock().expect("hits lock") += 1;
    match st.mode {
        JevMode::Ok => (StatusCode::OK, jev_ok_body().to_string()).into_response(),
        JevMode::Fail(code) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            "{}",
        )
            .into_response(),
    }
}

pub async fn spawn_jev(mode: JevMode) -> Mock {
    let hits = Arc::new(Mutex::new(0));
    let app = Router::new()
        .route("/v1/systemone", post(jev_handler))
        .with_state(JevState {
            mode,
            hits: hits.clone(),
        });
    let base = serve(app).await;
    Mock {
        base: format!("{base}/v1/systemone"),
        models: Arc::new(Mutex::new(Vec::new())),
        hits,
    }
}
