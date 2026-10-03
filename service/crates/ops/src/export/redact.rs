//! Secret redaction for exported configuration. Keys are always kept (so the owner can see what is
//! configured); values are replaced when the key or the value looks like a credential.
use serde_json::Value;

pub const REDACTED: &str = "[redacted]";

/// Substrings (lowercase) that mark a key as holding a credential.
const SECRET_KEY_MARKERS: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "credential",
    "private_key",
    "privatekey",
    "bearer",
    "cookie",
    "signature",
    "hmac",
];

/// Prefixes of well-known credential formats.
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk_live",
    "rk_live",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "AKIA",
    "ASIA",
    "AIza",
    "glpat-",
    "Bearer ",
    "Basic ",
];
const PRIVATE_KEY_MARKER: &str = "PRIVATE KEY";
const JWT_PREFIX: &str = "eyJ";
const JWT_SEGMENTS: usize = 3;

fn key_is_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEY_MARKERS.iter().any(|m| lower.contains(m))
}

pub fn value_is_secret(text: &str) -> bool {
    let t = text.trim();
    SECRET_VALUE_PREFIXES.iter().any(|p| t.starts_with(p))
        || t.contains(PRIVATE_KEY_MARKER)
        || (t.starts_with(JWT_PREFIX) && t.split('.').count() == JWT_SEGMENTS)
}

/// Returns a copy of `value` with secrets replaced by [`REDACTED`]. Keys are preserved.
pub fn redact(value: &Value) -> Value {
    redact_inner(value, false)
}

fn redact_inner(value: &Value, under_secret_key: bool) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        redact_inner(v, under_secret_key || key_is_secret(k)),
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| redact_inner(v, under_secret_key))
                .collect(),
        ),
        Value::String(s) if under_secret_key || value_is_secret(s) => {
            Value::String(REDACTED.into())
        }
        // Non-string scalars under a secret key (for example a numeric PIN) are redacted too.
        Value::Number(_) | Value::Bool(_) if under_secret_key => Value::String(REDACTED.into()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_redact_by_key_and_by_value_keeps_keys() {
        let v = json!({"api_token": "x", "label": "work", "n": {"k": "ghp_abc", "ok": 3}, "list": ["sk-1", "fine"]});
        let r = redact(&v);
        assert_eq!(r["api_token"], REDACTED);
        assert_eq!(r["label"], "work");
        assert_eq!(r["n"]["k"], REDACTED);
        assert_eq!(r["n"]["ok"], 3);
        assert_eq!(r["list"][0], REDACTED);
        assert_eq!(r["list"][1], "fine");
    }

    #[test]
    fn test_redact_secret_key_covers_whole_subtree() {
        let r = redact(&json!({"credentials": {"user": "a", "pin": 1234}}));
        assert_eq!(r["credentials"]["user"], REDACTED);
        assert_eq!(r["credentials"]["pin"], REDACTED);
    }

    #[test]
    fn test_value_is_secret_detects_jwt_and_pem() {
        assert!(value_is_secret("eyJhbGciOi.eyJzdWIiOi.sig"));
        assert!(value_is_secret("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(!value_is_secret("plain text"));
    }
}
