//! Local mock Jev server for tests. No live network.
use axum::{extract::State, http::StatusCode, routing::post, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub enum Behavior {
    Reply(Value),
    Status(u16),
    Slow(Duration, Value),
}

#[derive(Clone)]
pub struct Mock {
    pub url: String,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
    pub fn bodies(&self) -> Vec<String> {
        self.bodies.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

#[derive(Clone)]
struct AppState {
    behavior: Behavior,
    hits: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

async fn handler(State(s): State<AppState>, body: String) -> (StatusCode, String) {
    s.hits.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut b) = s.bodies.lock() {
        b.push(body);
    }
    match s.behavior {
        Behavior::Reply(v) => (StatusCode::OK, v.to_string()),
        Behavior::Status(code) => (StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), "{}".into()),
        Behavior::Slow(d, v) => {
            tokio::time::sleep(d).await;
            (StatusCode::OK, v.to_string())
        }
    }
}

pub async fn spawn(behavior: Behavior) -> Mock {
    let hits = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let state = AppState { behavior, hits: hits.clone(), bodies: bodies.clone() };
    let app = Router::new().route("/v1/systemone", post(handler)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Mock { url: format!("http://{addr}/v1/systemone"), hits, bodies }
}

/// A valid two-question response. `top_p` is the probability of the chosen label.
pub fn ok_body(intent: &str, difficulty: &str, top_p: f64) -> Value {
    let dist = |labels: &[&str], chosen: &str| -> Value {
        let rest = (1.0 - top_p) / (labels.len() as f64 - 1.0);
        let m: serde_json::Map<String, Value> =
            labels.iter().map(|l| ((*l).to_string(), json!(if *l == chosen { top_p } else { rest }))).collect();
        Value::Object(m)
    };
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "intent": {"type": "choice", "choice": intent, "confidence": top_p,
                "probabilities": dist(&crate::classification::questions::INTENT_LABELS, intent)},
            "difficulty": {"type": "choice", "choice": difficulty, "confidence": top_p,
                "probabilities": dist(&crate::classification::questions::DIFFICULTY_LABELS, difficulty)}
        },
        "usage": {"input_tokens": 300, "output_tokens": 20}
    })
}
