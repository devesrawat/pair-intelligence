//! Secret redaction. Applied to every error string that can leave a provider adapter.
use regex::Regex;
use std::sync::OnceLock;

pub const REDACTED: &str = "[REDACTED]";
/// Secrets shorter than this are not registered for literal matching (avoids mangling text).
const MIN_LITERAL_SECRET_LEN: usize = 6;

/// An API key or token. Never prints its value via `Debug`/`Display`.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Explicit access; call sites that use this are the audit surface.
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"sk-[A-Za-z0-9_\-]{8,}",
            r"(?i)bearer\s+[A-Za-z0-9._\-~+/=]{6,}",
            r#"(?i)(x-api-key|api[_-]?key|authorization|token|secret)["']?\s*[:=]\s*["']?[^\s"',;}]{4,}"#,
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    })
}

/// Redacts well-known key shapes plus any registered literal secrets.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    literals: Vec<String>,
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a new redactor that also scrubs this exact secret value.
    #[must_use]
    pub fn with_secret(&self, secret: &Secret) -> Self {
        let mut literals = self.literals.clone();
        if secret.expose().len() >= MIN_LITERAL_SECRET_LEN {
            literals.push(secret.expose().to_owned());
        }
        Self { literals }
    }

    pub fn redact(&self, input: &str) -> String {
        let mut out = input.to_owned();
        for lit in &self.literals {
            out = out.replace(lit.as_str(), REDACTED);
        }
        for re in patterns() {
            out = re.replace_all(&out, REDACTED).into_owned();
        }
        out
    }
}

/// Redact using pattern rules only.
pub fn redact_secrets(input: &str) -> String {
    Redactor::new().redact(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_patterns_scrub_known_shapes() {
        let s = "failed: x-api-key: sk-ant-abcdef123456 and Authorization: Bearer abc.def-ghi";
        let r = redact_secrets(s);
        assert!(!r.contains("abcdef123456"), "{r}");
        assert!(!r.contains("abc.def-ghi"), "{r}");
    }

    #[test]
    fn redact_literal_secret_removed_anywhere() {
        let sec = Secret::new("hunter2hunter2");
        let r = Redactor::new().with_secret(&sec).redact("url=https://x/?k=hunter2hunter2&z=1");
        assert!(!r.contains("hunter2"));
    }

    #[test]
    fn secret_debug_does_not_leak() {
        let sec = Secret::new("topsecretvalue");
        assert!(!format!("{sec:?} {sec}").contains("topsecret"));
    }
}
