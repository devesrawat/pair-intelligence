//! Content normalisation, topic derivation and risk heuristics (spec section 7: dedupe).

const MAX_TOPIC_TOKENS: usize = 8;
/// Separators that split "<topic> <separator> <value>". Earliest match wins.
const TOPIC_SEPARATORS: [&str; 9] = [
    ": ", " will use ", " should use ", " uses ", " prefers ", " chose ", " is ", " are ", " = ",
];
/// Words that make a statement permission-like; such candidates always need review.
const PERMISSION_WORDS: [&str; 12] = [
    "permission", "allow", "approve", "approval", "sudo", "credential", "password", "token",
    "always run", "never ask", "without asking", "auto-approve",
];

/// Lowercase alphanumeric tokens joined by single spaces.
pub fn normalize_content(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().collect::<String>()
            } else {
                " ".to_string()
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Heuristic subject of a statement, used only for contradiction detection.
/// Explicit topics from the extraction schema take precedence over this.
pub fn topic_of(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    let cut = TOPIC_SEPARATORS.iter().filter_map(|sep| lower.find(sep)).min()?;
    let topic = normalize_content(&lower[..cut]);
    let tokens = topic.split(' ').filter(|t| !t.is_empty()).count();
    (1..=MAX_TOPIC_TOKENS).contains(&tokens).then_some(topic)
}

pub fn looks_permission_like(content: &str) -> bool {
    let lower = content.to_lowercase();
    PERMISSION_WORDS.iter().any(|w| lower.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_content_punctuation_and_case_collapse() {
        assert_eq!(normalize_content("  Staging DB: Port 5432! "), "staging db port 5432");
    }

    #[test]
    fn test_topic_of_colon_form_returns_left_side() {
        assert_eq!(topic_of("Ledger storage: Postgres").as_deref(), Some("ledger storage"));
    }

    #[test]
    fn test_topic_of_no_separator_returns_none() {
        assert_eq!(topic_of("Postgres"), None);
    }

    #[test]
    fn test_looks_permission_like_flags_allow_statements() {
        assert!(looks_permission_like("Always run shell commands without asking"));
        assert!(!looks_permission_like("I prefer dark mode"));
    }
}
