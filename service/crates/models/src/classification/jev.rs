//! Jev (TypeSafe) adapter implementing `pair_core::traits::Classifier`.
//! One call, hard deadline, no synchronous retry. Output is normalised to `TaskClassification`
//! where `*_confidence` is the chosen option's probability (a PAIR convention, not a vendor statistic).
use super::jev_wire::{parse_response, validate_choice, ValidatedChoice};
use super::questions::{QuestionSet, DIFFICULTY_ID, INTENT_ID};
use super::state::{build_state, estimate_tokens, sha256_hex};
use crate::provider::guard::{guarded_client, GuardedResolver};
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::TaskId;
use pair_core::traits::Classifier;
use pair_core::types::{ClassificationInput, TaskClassification};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(1);
pub const API_KEY_ENV: &str = "TYPESAFE_API_KEY";

/// API key that never prints its value.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
    /// Read from `TYPESAFE_API_KEY`. Returns `Unauthenticated` when unset or empty.
    pub fn from_env() -> Result<Self> {
        match std::env::var(API_KEY_ENV) {
            Ok(v) if !v.trim().is_empty() => Ok(Self(v)),
            _ => Err(PairError::new(
                ErrorCode::Unauthenticated,
                format!("{API_KEY_ENV} is not set"),
            )),
        }
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey([REDACTED])")
    }
}

/// Stored for debugging. Never contains state text: only length and a SHA-256 of the redacted state.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifierDebugRecord {
    pub task: TaskId,
    pub request_id: String,
    pub model_version: String,
    pub question_version: String,
    pub state_chars: usize,
    pub state_sha256: String,
    pub intent: String,
    pub difficulty: String,
    pub input_tokens: u64,
    pub latency_ms: u64,
}

pub trait DebugSink: Send + Sync {
    fn record(&self, rec: ClassifierDebugRecord);
}

#[derive(Debug, Clone)]
pub struct JevSettings {
    pub endpoint: String,
    /// Requested model alias or version. Direct `jev-1.13.0` pinning is UNVERIFIED; the returned
    /// `model` is what gets recorded as `model_version`.
    pub model: String,
    pub deadline: Duration,
}

pub struct JevClassifier {
    http: reqwest::Client,
    settings: JevSettings,
    key: ApiKey,
    questions: QuestionSet,
    sink: Option<Arc<dyn DebugSink>>,
}

impl JevClassifier {
    pub fn new(settings: JevSettings, key: ApiKey, questions: QuestionSet) -> Result<Self> {
        let http = guarded_client(GuardedResolver::system())?;
        Ok(Self {
            http,
            settings,
            key,
            questions,
            sink: None,
        })
    }

    pub fn with_debug_sink(mut self, sink: Arc<dyn DebugSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn questions(&self) -> &QuestionSet {
        &self.questions
    }

    async fn call(&self, state: &str) -> Result<(String, Option<String>)> {
        let body = json!({"state": state, "model": self.settings.model, "questions": self.questions.to_wire()});
        let resp = self
            .http
            .post(&self.settings.endpoint)
            .bearer_auth(&self.key.0)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                PairError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("jev transport failed ({})", transport_kind(&e)),
                )
            })?;
        let status = resp.status();
        let request_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if !status.is_success() {
            // 429/529/5xx: no retry on the interactive path; caller falls back to the baseline.
            let code = match status.as_u16() {
                401 => ErrorCode::Unauthenticated,
                422 => ErrorCode::ClassifierInvalid,
                _ => ErrorCode::ProviderUnavailable,
            };
            return Err(PairError::new(
                code,
                format!("jev returned HTTP {}", status.as_u16()),
            ));
        }
        let text = resp.text().await.map_err(|e| {
            PairError::new(
                ErrorCode::ProviderUnavailable,
                format!("jev body unreadable ({})", transport_kind(&e)),
            )
        })?;
        Ok((text, request_id))
    }

    fn normalise(
        &self,
        text: &str,
        request_id: Option<String>,
        latency_ms: u64,
    ) -> Result<TaskClassification> {
        let resp = parse_response(text)?;
        let pick = |id: &str| -> Result<ValidatedChoice> {
            let answer = resp.answers.get(id).ok_or_else(|| {
                PairError::new(ErrorCode::ClassifierInvalid, format!("missing answer {id}"))
            })?;
            validate_choice(id, answer, &self.questions.labels(id))
        };
        let intent = pick(INTENT_ID)?;
        let difficulty = pick(DIFFICULTY_ID)?;
        Ok(TaskClassification {
            model_version: resp.model,
            question_version: self.questions.question_version.clone(),
            intent: intent.label,
            difficulty: difficulty.label,
            intent_probabilities: intent.probabilities,
            difficulty_probabilities: difficulty.probabilities,
            intent_confidence: intent.confidence,
            difficulty_confidence: difficulty.confidence,
            input_tokens: resp.usage.input_tokens,
            latency_ms,
            request_id: request_id.unwrap_or_else(|| format!("local-{}", Uuid::now_v7())),
        })
    }
}

/// Coarse failure class for error messages. `reqwest::Error` text embeds the request URL
/// (and its query), so it is never forwarded.
fn transport_kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_redirect() {
        "redirect"
    } else if e.is_decode() || e.is_body() {
        "body"
    } else {
        "request"
    }
}

#[async_trait]
impl Classifier for JevClassifier {
    async fn classify(&self, input: ClassificationInput) -> Result<TaskClassification> {
        let state = build_state(&input);
        let started = Instant::now();
        let outcome = tokio::time::timeout(self.settings.deadline, self.call(&state)).await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (text, request_id) = match outcome {
            Ok(r) => r?,
            Err(_) => {
                return Err(PairError::new(
                    ErrorCode::ProviderTimeout,
                    format!(
                        "jev exceeded {} ms deadline",
                        self.settings.deadline.as_millis()
                    ),
                ))
            }
        };
        let classification = self.normalise(&text, request_id, latency_ms)?;
        tracing::debug!(
            task = %input.task, request_id = %classification.request_id,
            model_version = %classification.model_version, latency_ms,
            "jev classification"
        );
        if let Some(sink) = &self.sink {
            sink.record(ClassifierDebugRecord {
                task: input.task,
                request_id: classification.request_id.clone(),
                model_version: classification.model_version.clone(),
                question_version: classification.question_version.clone(),
                state_chars: state.chars().count(),
                state_sha256: sha256_hex(&state),
                intent: classification.intent.clone(),
                difficulty: classification.difficulty.clone(),
                input_tokens: classification.input_tokens,
                latency_ms,
            });
        }
        Ok(classification)
    }
}

/// Worst-case input tokens for budget reservation before the call.
pub fn estimate_input_tokens(input: &ClassificationInput, questions: &QuestionSet) -> u64 {
    estimate_tokens(&build_state(input), questions.wire_chars())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::questions::test_questions;
    use crate::classification::test_support::{ok_body, spawn, Behavior};
    use std::sync::Mutex;

    fn classifier(url: &str, deadline_ms: u64) -> JevClassifier {
        let settings = JevSettings {
            endpoint: url.into(),
            model: "jev-1.13.0".into(),
            deadline: Duration::from_millis(deadline_ms),
        };
        JevClassifier::new(settings, ApiKey::new("test-key"), test_questions()).expect("client")
    }

    fn input(request: &str, summary: &str) -> ClassificationInput {
        ClassificationInput {
            request: request.into(),
            recent_summary: summary.into(),
            project_type: Some("rust-service".into()),
            workflows: vec!["engineering".into(), "research".into()],
            task: TaskId::new(),
        }
    }

    #[derive(Default)]
    struct Collect(Mutex<Vec<ClassifierDebugRecord>>);
    impl DebugSink for Collect {
        fn record(&self, rec: ClassifierDebugRecord) {
            if let Ok(mut v) = self.0.lock() {
                v.push(rec);
            }
        }
    }

    #[tokio::test]
    async fn test_classify_valid_response_normalises_confidence_to_top_probability() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "routine", 0.9))).await;
        let sink = Arc::new(Collect::default());
        let c = classifier(&mock.url, 1000).with_debug_sink(sink.clone());
        let out = c
            .classify(input("fix the typo", ""))
            .await
            .expect("classified");
        assert_eq!(out.intent, "coding");
        assert!((out.intent_confidence - 0.9).abs() < 1e-9);
        assert_eq!(out.model_version, "jev-1.13.0");
        assert_eq!(out.input_tokens, 300);
        let recs = sink.0.lock().expect("lock");
        assert_eq!(recs.len(), 1);
        assert!(!format!("{:?}", recs[0]).contains("fix the typo"));
    }

    #[tokio::test]
    async fn invalid_label_is_rejected_at_adapter_boundary() {
        let mut body = ok_body("coding", "routine", 0.9);
        body["answers"]["intent"]["choice"] = json!("banana");
        let mock = spawn(Behavior::Reply(body)).await;
        let err = classifier(&mock.url, 1000)
            .classify(input("x", ""))
            .await
            .expect_err("invalid");
        assert_eq!(err.code, ErrorCode::ClassifierInvalid);
    }

    #[tokio::test]
    async fn test_classify_probability_out_of_range_rejected() {
        let mut body = ok_body("coding", "routine", 0.9);
        body["answers"]["difficulty"]["probabilities"]["deep"] = json!(1.7);
        let mock = spawn(Behavior::Reply(body)).await;
        let err = classifier(&mock.url, 1000)
            .classify(input("x", ""))
            .await
            .expect_err("invalid");
        assert_eq!(err.code, ErrorCode::ClassifierInvalid);
    }

    #[tokio::test]
    async fn no_sync_retry_on_429() {
        let mock = spawn(Behavior::Status(429)).await;
        let err = classifier(&mock.url, 1000)
            .classify(input("x", ""))
            .await
            .expect_err("429");
        assert_eq!(err.code, ErrorCode::ProviderUnavailable);
        assert_eq!(mock.hits(), 1, "exactly one request, no synchronous retry");
    }

    #[tokio::test]
    async fn test_classify_slow_server_times_out_without_retry() {
        let mock = spawn(Behavior::Slow(
            Duration::from_millis(400),
            ok_body("coding", "routine", 0.9),
        ))
        .await;
        let err = classifier(&mock.url, 50)
            .classify(input("x", ""))
            .await
            .expect_err("timeout");
        assert_eq!(err.code, ErrorCode::ProviderTimeout);
        assert_eq!(mock.hits(), 1);
    }

    #[tokio::test]
    async fn followup_includes_relevant_context() {
        let mock = spawn(Behavior::Reply(ok_body("coding", "routine", 0.9))).await;
        let summary =
            "User asked to fix the flaky reconcile test in crates/budget. Key: sk-live-SECRET123";
        classifier(&mock.url, 1000)
            .classify(input("do it", summary))
            .await
            .expect("ok");
        let bodies = mock.bodies();
        let sent = bodies.first().expect("one request");
        assert!(sent.contains("do it"));
        assert!(
            sent.contains("flaky reconcile test"),
            "recent summary must travel with a follow-up"
        );
        assert!(sent.contains("rust-service") && sent.contains("engineering"));
        assert!(
            !sent.contains("sk-live-SECRET123"),
            "credentials must be excluded"
        );
        assert!(sent.contains("\"criteria\""));
    }

    #[tokio::test]
    async fn jev_client_does_not_follow_redirects() {
        let target = spawn(Behavior::Reply(ok_body("coding", "routine", 0.9))).await;
        let redirector = spawn(Behavior::Redirect(target.url.clone())).await;
        let err = classifier(&redirector.url, 1000)
            .classify(input("secret request body", ""))
            .await
            .expect_err("307 is an error, not followed");
        assert_eq!(err.code, ErrorCode::ProviderUnavailable);
        assert_eq!(redirector.hits(), 1);
        assert_eq!(
            target.hits(),
            0,
            "body must not be re-sent to the redirect target"
        );
    }

    #[tokio::test]
    async fn jev_error_redacts_url_and_key() {
        const KEY: &str = "sk-very-secret-key";
        let url = "http://127.0.0.1:1/private-path?token=URLSECRET";
        let settings = JevSettings {
            endpoint: url.into(),
            model: "jev-1.13.0".into(),
            deadline: Duration::from_millis(1000),
        };
        let c = JevClassifier::new(settings, ApiKey::new(KEY), test_questions()).expect("client");
        let err = c.classify(input("x", "")).await.expect_err("refused");
        let text = format!("{err} {err:?}");
        for leaked in [KEY, "private-path", "URLSECRET", "127.0.0.1"] {
            assert!(!text.contains(leaked), "{leaked} leaked in {text}");
        }
    }

    #[test]
    fn test_api_key_debug_redacted() {
        assert!(!format!("{:?}", ApiKey::new("super-secret")).contains("super-secret"));
    }
}
