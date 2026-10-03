//! Ollama Cloud adapter: `POST {base}/api/chat` with Bearer auth, NDJSON streaming.
//! The base URL comes from the registry entry and must pass the cloud-only guard.
use super::common::{
    admit_subscription, allow_unverified_from_env, http_error, net_error, read_lines, usage_report,
    vet_request, wire_role, with_deadline,
};
use super::guard::{guarded_client, GuardedResolver};
use super::limits::{now_epoch, Block, LimitGate};
use super::quota::QuotaLimiter;
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

pub const OLLAMA_KEY_ENV: &str = "OLLAMA_API_KEY";
const CHAT_PATH: &str = "/api/chat";
/// Backoff after a 429 that carries no usable `Retry-After`.
const DEFAULT_RETRY_AFTER_SECS: u64 = 60;

pub struct OllamaCloudProvider {
    client: reqwest::Client,
    key: Secret,
    registry: Arc<ProviderRegistry>,
    redactor: Redactor,
    allow_unverified: bool,
    quota: QuotaLimiter,
    gate: LimitGate,
}

impl std::fmt::Debug for OllamaCloudProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaCloudProvider")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl OllamaCloudProvider {
    pub fn new(key: Secret, registry: Arc<ProviderRegistry>) -> Result<Self> {
        if key.is_empty() {
            return Err(PairError::new(
                ErrorCode::Unauthenticated,
                "empty Ollama API key",
            ));
        }
        let client = guarded_client(GuardedResolver::system())?;
        let redactor = Redactor::new().with_secret(&key);
        Ok(Self {
            client,
            key,
            registry,
            redactor,
            allow_unverified: allow_unverified_from_env(),
            quota: QuotaLimiter::per_minute(),
            gate: LimitGate::default(),
        })
    }

    /// Override the `PAIR_ALLOW_UNVERIFIED_MODEL_IDS` environment decision.
    #[must_use]
    pub fn with_allow_unverified_ids(mut self, allow: bool) -> Self {
        self.allow_unverified = allow;
        self
    }

    pub fn from_env(registry: Arc<ProviderRegistry>) -> Result<Self> {
        let key = std::env::var(OLLAMA_KEY_ENV).map_err(|_| {
            PairError::new(
                ErrorCode::Unauthenticated,
                format!("{OLLAMA_KEY_ENV} is not set"),
            )
        })?;
        Self::new(Secret::new(key), registry)
    }

    async fn call(&self, entry: &ModelEntry, req: &ModelRequest) -> Result<ModelResponse> {
        let started = Instant::now();
        let messages = chat_messages(&req.messages)?;
        let body = json!({
            "model": entry.id,
            "messages": messages,
            "stream": true,
            "options": {"num_predict": req.max_output_tokens},
        });
        let resp = self
            .client
            .post(entry.endpoint.url(CHAT_PATH))
            .bearer_auth(self.key.expose())
            .header(TRACE_HEADER, req.trace.0.to_string())
            .json(&body)
            .send()
            .await
            .map_err(|e| net_error(&self.redactor, e))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.gate.record(Some(Block {
                until: now_epoch() + retry_after_secs(&resp),
                reason: "rate limited (HTTP 429)".to_owned(),
            }));
        }
        if !resp.status().is_success() {
            return Err(http_error(&self.redactor, resp).await);
        }
        let mut acc = Accumulator::default();
        let done = read_lines(&self.redactor, resp, |line| acc.apply_line(line)).await;
        let done = done.map_err(|e| PairError::new(e.code, self.redactor.redact(&e.message)))?;
        if !done {
            return Err(PairError::new(
                ErrorCode::ProviderUnavailable,
                "stream ended before done=true",
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
            provider_request_id: None,
            latency_ms,
        })
    }
}

#[async_trait]
impl Provider for OllamaCloudProvider {
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse> {
        let entry = vet_request(
            &self.registry,
            &req,
            ProviderKind::OllamaCloud,
            self.allow_unverified,
        )?;
        self.gate.check(&entry.id)?;
        admit_subscription(&entry, &self.quota)?;
        let trace = req.trace;
        let result = with_deadline(req.deadline_ms, self.call(&entry, &req)).await;
        match &result {
            Ok(r) => {
                info!(%trace, model = %r.resolved_model, latency_ms = r.latency_ms, "ollama cloud call ok")
            }
            Err(e) => warn!(%trace, code = ?e.code, "ollama cloud call failed"),
        }
        result
    }
}

/// Seconds the provider asked us to wait. Only the delta-seconds form is read; an HTTP-date or a
/// missing header falls back to a conservative minute.
fn retry_after_secs(resp: &reqwest::Response) -> u64 {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
}

fn chat_messages(msgs: &[ModelMessage]) -> Result<Vec<serde_json::Value>> {
    msgs.iter()
        .map(|m| Ok(json!({"role": wire_role(m)?, "content": m.content})))
        .collect()
}

#[derive(Debug, Deserialize)]
struct Chunk {
    model: Option<String>,
    message: Option<ChunkMessage>,
    #[serde(default)]
    done: bool,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkMessage {
    content: Option<String>,
}

#[derive(Debug, Default)]
struct Accumulator {
    text: String,
    model: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
}

impl Accumulator {
    fn apply_line(&mut self, line: &str) -> Result<bool> {
        if line.trim().is_empty() {
            return Ok(false);
        }
        let c: Chunk = serde_json::from_str(line).map_err(|e| {
            PairError::new(
                ErrorCode::ProviderUnavailable,
                format!("malformed NDJSON chunk: {e}"),
            )
        })?;
        if let Some(err) = c.error {
            return Err(PairError::new(
                ErrorCode::ProviderUnavailable,
                format!("provider stream error: {err}"),
            ));
        }
        if let Some(m) = c.model {
            self.model = Some(m);
        }
        if let Some(t) = c.message.and_then(|m| m.content) {
            self.text.push_str(&t);
        }
        if c.done {
            self.input_tokens = c.prompt_eval_count.unwrap_or(0);
            self.output_tokens = c.eval_count.unwrap_or(0);
        }
        Ok(c.done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pair_core::types::{ModelMessage, TrustClass};

    #[test]
    fn untrusted_message_never_sent_as_system() {
        let msg = |role: &str, trust| ModelMessage {
            role: role.into(),
            content: format!("{role}-text"),
            trust,
        };
        let wire = chat_messages(&[
            msg("system", TrustClass::Owner),
            msg("system", TrustClass::Untrusted),
            msg("assistant", TrustClass::Tool),
        ])
        .expect("messages");
        let roles: Vec<_> = wire.iter().map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, [Some("system"), Some("user"), Some("user")]);
    }
}
