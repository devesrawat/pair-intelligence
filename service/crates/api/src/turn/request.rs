//! `POST /v1/turn` request validation. Nothing here touches the network or the database.

use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ConversationId, TaskId};
use pair_core::types::{DataClass, TaskKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const MAX_MESSAGE_CHARS: usize = 16_000;
const MAX_CLIENT_MESSAGE_ID_LEN: usize = 128;
const MAX_PROJECT_TYPE_LEN: usize = 64;
const TASK_ID_DOMAIN: &str = "pair-turn-task";
const CLASSIFIER_ID_DOMAIN: &str = "pair-turn-classifier";

/// Data classes a turn may carry. Employer and sensitive data reach no provider until the owner
/// authorizes one for them (spec global constraints), so they are refused at the door.
const TURN_DATA_CLASSES: [DataClass; 2] = [DataClass::Public, DataClass::Personal];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRequest {
    pub conversation_id: Option<Uuid>,
    /// Idempotency handle: with `conversation_id` it fixes the task id, so retries share one
    /// attempt counter and one task budget instead of each looking like a new task.
    pub client_message_id: Option<String>,
    pub message: String,
    pub data_class: String,
    #[serde(default = "default_kind")]
    pub kind: TaskKind,
    pub project_type: Option<String>,
}

fn default_kind() -> TaskKind {
    TaskKind::Default
}

#[derive(Debug, Clone)]
pub struct ValidTurn {
    pub conversation: Option<ConversationId>,
    pub client_message_id: Option<String>,
    pub message: String,
    pub data_class: DataClass,
    pub kind: TaskKind,
    pub project_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TurnResponse {
    pub text: String,
    pub resolved_model: String,
    pub route_reason: String,
    /// `reconciled`, or `unresolved` when the cost is unknown and the reservation stays counted.
    pub cost_state: &'static str,
    pub trace_id: String,
    pub conversation_id: String,
    pub message_id: String,
}

fn parse_data_class(raw: &str) -> Result<DataClass> {
    let class: DataClass = serde_json::from_value(serde_json::Value::String(raw.to_owned()))
        .map_err(|_| {
            PairError::new(
                ErrorCode::PolicyDenied,
                "unknown data class; refusing (fail closed)",
            )
        })?;
    if TURN_DATA_CLASSES.contains(&class) {
        Ok(class)
    } else {
        Err(PairError::new(
            ErrorCode::ProviderDisallowed,
            format!("no provider is authorized for {class:?} data"),
        ))
    }
}

fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_CLIENT_MESSAGE_ID_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

impl TurnRequest {
    /// The data class is judged first, before any other field, so a refused class never gets as
    /// far as a classifier call, a database write or a provider.
    pub fn validate(self) -> Result<ValidTurn> {
        let data_class = parse_data_class(&self.data_class)?;
        let message = self.message.trim().to_owned();
        if message.is_empty() || message.chars().count() > MAX_MESSAGE_CHARS {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("message must be 1..={MAX_MESSAGE_CHARS} characters"),
            ));
        }
        if let Some(id) = &self.client_message_id {
            if !safe_id(id) {
                return Err(PairError::new(
                    ErrorCode::InvalidInput,
                    "client_message_id must be 1..=128 characters of [A-Za-z0-9._:-]",
                ));
            }
        }
        if self
            .project_type
            .as_deref()
            .is_some_and(|p| p.is_empty() || p.len() > MAX_PROJECT_TYPE_LEN)
        {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "project_type must be 1..=64 characters",
            ));
        }
        Ok(ValidTurn {
            conversation: self.conversation_id.map(ConversationId),
            client_message_id: self.client_message_id,
            message,
            data_class,
            kind: self.kind,
            project_type: self.project_type,
        })
    }
}

fn derived_uuid(domain: &str, parts: &[&str]) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for p in parts {
        hasher.update([0u8]);
        hasher.update(p.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

impl ValidTurn {
    /// Stable per logical turn when the caller supplies `client_message_id`; random otherwise.
    pub fn task_id(&self) -> TaskId {
        match (&self.conversation, &self.client_message_id) {
            (Some(c), Some(m)) => TaskId(derived_uuid(
                TASK_ID_DOMAIN,
                &[&c.0.to_string(), m.as_str()],
            )),
            _ => TaskId::new(),
        }
    }
}

/// The classifier gets its own task id: `ReserveRequest::classifier` registers its task as a
/// `default` task, and a later `coding`/`research` reservation on the same id would be a Conflict.
pub fn classifier_task(task: TaskId) -> TaskId {
    TaskId(derived_uuid(CLASSIFIER_ID_DOMAIN, &[&task.0.to_string()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(class: &str) -> TurnRequest {
        TurnRequest {
            conversation_id: None,
            client_message_id: None,
            message: "hello".into(),
            data_class: class.into(),
            kind: TaskKind::Default,
            project_type: None,
        }
    }

    #[test]
    fn test_validate_employer_and_sensitive_refused_with_provider_disallowed() {
        for class in ["employer", "sensitive"] {
            let err = req(class).validate().expect_err(class);
            assert_eq!(err.code, ErrorCode::ProviderDisallowed, "{class}");
        }
    }

    #[test]
    fn test_validate_unknown_data_class_refused_with_policy_denied() {
        for class in ["", "secret", "PUBLIC", "public "] {
            let err = req(class).validate().expect_err(class);
            assert_eq!(err.code, ErrorCode::PolicyDenied, "{class:?}");
        }
    }

    #[test]
    fn test_validate_class_is_judged_before_message_shape() {
        let mut r = req("employer");
        r.message = String::new();
        assert_eq!(
            r.validate().expect_err("refused").code,
            ErrorCode::ProviderDisallowed
        );
    }

    #[test]
    fn test_task_id_stable_for_same_conversation_and_client_message_id() {
        let conv = ConversationId::new();
        let mk = |m: &str| {
            let mut r = req("public");
            r.conversation_id = Some(conv.0);
            r.client_message_id = Some(m.into());
            r.validate().expect("valid").task_id()
        };
        assert_eq!(mk("a").0, mk("a").0);
        assert_ne!(mk("a").0, mk("b").0);
        assert_ne!(req("public").validate().expect("v").task_id().0, mk("a").0);
    }

    #[test]
    fn test_classifier_task_differs_from_turn_task_and_is_stable() {
        let t = TaskId::new();
        assert_ne!(classifier_task(t).0, t.0);
        assert_eq!(classifier_task(t).0, classifier_task(t).0);
    }
}
