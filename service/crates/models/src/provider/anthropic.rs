//! Anthropic Messages API adapter (streaming SSE). Cancel by dropping the `generate` future.
use super::common::{http_error, net_error, read_lines, usage_report, vet_request, with_deadline};
use super::registry::{ModelEntry, ProviderKind, ProviderRegistry};
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::traits::Provider;
use pair_core::types::{ModelMessage, ModelRequest, ModelResponse};
use pair_telemetry::{Redactor, Secret, TRACE_HEADER};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const MESSAGES_PATH: &str = "/v1/messages";
const REQUEST_ID_HEADER: &str = "request-id";

pub struct AnthropicProvider {
    client: reqwest::Client,
    key: Secret,
    registry: Arc<ProviderRegistry>,
    redactor: Redactor,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    pub fn new(key: Secret, registry: Arc<ProviderRegistry>) -> Result<Self> {
        if key.is_empty() {
            return Err(PairError::new(
                ErrorCode::Unauthenticated,
                "empty Anthropic API key",
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| PairError::new(ErrorCode::Internal, format!("http client: {e}")))?;
        let redactor = Redactor::new().with_secret(&key);
        Ok(Self {
            client,
            key,
            registry,
            redactor,
        })
    }

    /// Read the key from `ANTHROPIC_API_KEY` (environment only).
    pub fn from_env(registry: Arc<ProviderRegistry>) -> Result<Self> {
        let key = std::env::var(ANTHROPIC_KEY_ENV).map_err(|_| {
            PairError::new(
                ErrorCode::Unauthenticated,
                format!("{ANTHROPIC_KEY_ENV} is not set"),
            )
        })?;
        Self::new(Secret::new(key), registry)
    }

    async fn call(&self, entry: &ModelEntry, req: &ModelRequest) -> Result<ModelResponse> {
        let started = Instant::now();
        let body = build_body(entry, req)?;
        let resp = self
            .client
            .post(entry.endpoint.url(MESSAGES_PATH))
            .header("x-api-key", self.key.expose())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(TRACE_HEADER, req.trace.0.to_string())
            .json(&body)
            .send()
            .await
            .map_err(|e| net_error(&self.redactor, e))?;
        if !resp.status().is_success() {
            return Err(http_error(&self.redactor, resp).await);
        }
        let request_id = resp
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut acc = StreamAccumulator::default();
        let done = read_lines(&self.redactor, resp, |line| acc.apply_line(line)).await;
        let done = done.map_err(|e| PairError::new(e.code, self.redactor.redact(&e.message)))?;
        if !done {
            return Err(PairError::new(
                ErrorCode::ProviderUnavailable,
                "stream ended before message_stop",
            ));
        }
        let price = entry
            .price
            .as_ref()
            .ok_or_else(|| PairError::new(ErrorCode::BudgetUnknownPrice, "no price"))?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(ModelResponse {
            resolved_model: acc.model.unwrap_or_else(|| req.model_id.clone()),
            text: acc.text,
            usage: usage_report(price, acc.input_tokens, acc.output_tokens),
            provider_request_id: request_id.or(acc.message_id),
            latency_ms,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse> {
        let entry = vet_request(&self.registry, &req, ProviderKind::Anthropic)?;
        let trace = req.trace;
        let result = with_deadline(req.deadline_ms, self.call(&entry, &req)).await;
        match &result {
            Ok(r) => {
                info!(%trace, model = %r.resolved_model, latency_ms = r.latency_ms, "anthropic call ok")
            }
            Err(e) => warn!(%trace, code = ?e.code, "anthropic call failed"),
        }
        result
    }
}

fn build_body(entry: &ModelEntry, req: &ModelRequest) -> Result<serde_json::Value> {
    let (system, messages) = split_messages(&req.messages)?;
    let mut body = json!({
        "model": entry.id,
        "max_tokens": req.max_output_tokens,
        "stream": true,
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    Ok(body)
}

fn split_messages(msgs: &[ModelMessage]) -> Result<(String, Vec<serde_json::Value>)> {
    let mut system = Vec::new();
    let mut out = Vec::new();
    for m in msgs {
        match m.role.as_str() {
            "system" => system.push(m.content.as_str()),
            "user" | "assistant" => out.push(json!({"role": m.role, "content": m.content})),
            other => {
                return Err(PairError::new(
                    ErrorCode::InvalidInput,
                    format!("unsupported role {other}"),
                ))
            }
        }
    }
    if out.is_empty() {
        return Err(PairError::new(
            ErrorCode::InvalidInput,
            "request has no user/assistant messages",
        ));
    }
    Ok((system.join("\n\n"), out))
}

#[derive(Debug, Default)]
struct StreamAccumulator {
    text: String,
    model: Option<String>,
    message_id: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    message: Option<MessageStart>,
    delta: Option<serde_json::Value>,
    usage: Option<Usage>,
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<String>,
}

impl StreamAccumulator {
    /// Returns `Ok(true)` once `message_stop` is seen.
    fn apply_line(&mut self, line: &str) -> Result<bool> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(false);
        };
        let ev: Event = serde_json::from_str(data.trim()).map_err(|e| {
            PairError::new(
                ErrorCode::ProviderUnavailable,
                format!("malformed SSE event: {e}"),
            )
        })?;
        match ev.kind.as_str() {
            "message_start" => {
                if let Some(m) = ev.message {
                    self.model = m.model;
                    self.message_id = m.id;
                    self.apply_usage(m.usage);
                }
            }
            "content_block_delta" => {
                let piece = ev
                    .delta
                    .as_ref()
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str());
                if let Some(t) = piece {
                    self.text.push_str(t);
                }
            }
            "message_delta" => self.apply_usage(ev.usage),
            "message_stop" => return Ok(true),
            "error" => {
                let (kind, msg) = ev.error.map(|e| (e.kind, e.message)).unwrap_or_default();
                return Err(PairError::new(
                    ErrorCode::ProviderUnavailable,
                    format!(
                        "provider stream error {}: {}",
                        kind.unwrap_or_default(),
                        msg.unwrap_or_default()
                    ),
                ));
            }
            _ => {}
        }
        Ok(false)
    }

    fn apply_usage(&mut self, usage: Option<Usage>) {
        if let Some(u) = usage {
            self.input_tokens = u.input_tokens.unwrap_or(self.input_tokens);
            self.output_tokens = u.output_tokens.unwrap_or(self.output_tokens);
        }
    }
}
