//! Compact classifier state: current request + short recent summary + project type + workflows.
//! Credentials are redacted and sizes bounded; full repositories are never included.
use pair_core::types::ClassificationInput;
use sha2::{Digest, Sha256};

pub const MAX_REQUEST_CHARS: usize = 2_000;
pub const MAX_SUMMARY_CHARS: usize = 1_000;
pub const MAX_WORKFLOWS: usize = 12;
const CHARS_PER_TOKEN: usize = 4;
const REDACTED: &str = "[REDACTED]";
const SECRET_PREFIXES: [&str; 8] = ["sk-", "ghp_", "gho_", "xoxb-", "xoxp-", "akia", "eyj", "-----begin"];
const SECRET_KEY_HINTS: [&str; 6] = ["key", "token", "secret", "password", "passwd", "credential"];
const LONG_OPAQUE_LEN: usize = 32;

fn looks_secret(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    if SECRET_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    if let Some((k, v)) = lower.split_once(['=', ':']) {
        if !v.is_empty() && SECRET_KEY_HINTS.iter().any(|h| k.contains(h)) {
            return true;
        }
    }
    word.len() >= LONG_OPAQUE_LEN
        && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

/// Replace credential-shaped words (provider key prefixes, `key=value`, long opaque tokens,
/// bearer tokens) with a marker. Whitespace is normalised to single spaces within lines.
pub fn redact_secrets(text: &str) -> String {
    text.lines()
        .map(|line| {
            let mut bearer = false;
            line.split_whitespace()
                .map(|w| {
                    let hide = bearer || looks_secret(w);
                    bearer = w.eq_ignore_ascii_case("bearer");
                    if hide { REDACTED } else { w }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn bounded(text: &str, max_chars: usize) -> String {
    let clean = redact_secrets(text);
    if clean.chars().count() <= max_chars {
        return clean;
    }
    let mut cut: String = clean.chars().take(max_chars).collect();
    cut.push_str(" [truncated]");
    cut
}

/// Build the state text sent to the classifier.
pub fn build_state(input: &ClassificationInput) -> String {
    let workflows: Vec<&str> = input.workflows.iter().take(MAX_WORKFLOWS).map(String::as_str).collect();
    let recent = if input.recent_summary.trim().is_empty() {
        "(none)".to_string()
    } else {
        bounded(&input.recent_summary, MAX_SUMMARY_CHARS)
    };
    format!(
        "CURRENT REQUEST (untrusted user text):\n{}\n\nRECENT CONTEXT SUMMARY:\n{}\n\nPROJECT TYPE: {}\nAVAILABLE WORKFLOWS: {}",
        bounded(&input.request, MAX_REQUEST_CHARS),
        recent,
        input.project_type.as_deref().map_or_else(|| "unknown".to_string(), |p| bounded(p, 64)),
        workflows.join(", "),
    )
}

/// Upper-bound token estimate for state plus question text (4 chars/token, rounded up).
pub fn estimate_tokens(state: &str, question_chars: usize) -> u64 {
    let chars = state.chars().count() + question_chars;
    chars.div_ceil(CHARS_PER_TOKEN) as u64
}

pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pair_core::ids::TaskId;

    fn input(request: &str, summary: &str) -> ClassificationInput {
        ClassificationInput {
            request: request.into(),
            recent_summary: summary.into(),
            project_type: Some("rust-service".into()),
            workflows: vec!["engineering".into(), "research".into()],
            task: TaskId::new(),
        }
    }

    #[test]
    fn test_redact_secrets_credentials_removed() {
        let out = redact_secrets("use sk-abc123 and API_KEY=hunter2 then Bearer tok_999 ok");
        assert!(!out.contains("sk-abc123") && !out.contains("hunter2") && !out.contains("tok_999"));
        assert!(out.ends_with("ok"));
    }

    #[test]
    fn test_build_state_truncates_long_request() {
        let state = build_state(&input(&"a ".repeat(5_000), ""));
        assert!(state.chars().count() < MAX_REQUEST_CHARS + 400);
        assert!(state.contains("[truncated]"));
    }
}
