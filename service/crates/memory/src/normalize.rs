//! Content normalisation, topic derivation and risk heuristics (spec section 7: dedupe).
use unicode_normalization::UnicodeNormalization;

const MAX_TOPIC_TOKENS: usize = 8;
/// Separators that split "<topic> <separator> <value>". Earliest match wins.
const TOPIC_SEPARATORS: [&str; 9] = [
    ": ",
    " will use ",
    " should use ",
    " uses ",
    " prefers ",
    " chose ",
    " is ",
    " are ",
    " = ",
];
/// Words that make a statement permission-like; such candidates always need review.
const PERMISSION_WORDS: [&str; 12] = [
    "permission",
    "allow",
    "approve",
    "approval",
    "sudo",
    "credential",
    "password",
    "token",
    "always run",
    "never ask",
    "without asking",
    "auto-approve",
];

/// NFC, lowercase, every non-alphanumeric character a separator, tokens joined by single spaces.
/// This is the plain-text form used for retrieval terms; `normalize_content` builds on it.
pub fn normalize_search_text(s: &str) -> String {
    let cleaned: String = s
        .nfc()
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

/// Identity form of a statement for dedupe and contradiction checks. Like `normalize_search_text`,
/// but the symbols that change meaning are spelled out so `C++`, `C#` and `C` stay distinct and
/// `-1` stays distinct from `1`.
pub fn normalize_content(s: &str) -> String {
    let nfc: Vec<char> = s.nfc().collect();
    let mut spelled = String::with_capacity(nfc.len());
    for (i, &c) in nfc.iter().enumerate() {
        match c {
            '+' => spelled.push_str(" plus "),
            '#' => spelled.push_str(" sharp "),
            '-' => {
                let prev_alnum = i > 0 && nfc[i - 1].is_alphanumeric();
                let next_digit = nfc.get(i + 1).is_some_and(char::is_ascii_digit);
                spelled.push_str(if next_digit && !prev_alnum {
                    " minus "
                } else {
                    " "
                });
            }
            other => spelled.push(other),
        }
    }
    normalize_search_text(&spelled)
}

/// Heuristic subject of a statement, used only for contradiction detection.
/// Explicit topics from the extraction schema take precedence over this.
pub fn topic_of(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    let cut = TOPIC_SEPARATORS
        .iter()
        .filter_map(|sep| lower.find(sep))
        .min()?;
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
        assert_eq!(
            normalize_content("  Staging DB: Port 5432! "),
            "staging db port 5432"
        );
    }

    #[test]
    fn test_normalize_content_c_family_does_not_collide() {
        let forms = ["C++", "C#", "C"].map(normalize_content);
        assert_ne!(forms[0], forms[1]);
        assert_ne!(forms[0], forms[2]);
        assert_ne!(forms[1], forms[2]);
        assert_ne!(
            normalize_content("I prefer C++ for services"),
            normalize_content("I prefer C for services")
        );
    }

    #[test]
    fn test_normalize_content_negative_number_differs_from_positive() {
        assert_ne!(
            normalize_content("offset: -1"),
            normalize_content("offset: 1")
        );
        // A hyphen inside a word or range is still just a separator.
        assert_eq!(normalize_content("2024-01-05"), "2024 01 05");
        assert_eq!(normalize_content("pages 1-5"), "pages 1 5");
    }

    #[test]
    fn test_normalize_content_nfc_forms_collapse() {
        assert_eq!(
            normalize_content("caf\u{e9}"),
            normalize_content("cafe\u{301}")
        );
    }

    #[test]
    fn test_normalize_search_text_keeps_plain_alphanumeric_terms() {
        assert_eq!(normalize_search_text("C++ style!"), "c style");
    }

    #[test]
    fn test_topic_of_colon_form_returns_left_side() {
        assert_eq!(
            topic_of("Ledger storage: Postgres").as_deref(),
            Some("ledger storage")
        );
    }

    #[test]
    fn test_topic_of_no_separator_returns_none() {
        assert_eq!(topic_of("Postgres"), None);
    }

    #[test]
    fn test_looks_permission_like_flags_allow_statements() {
        assert!(looks_permission_like(
            "Always run shell commands without asking"
        ));
        assert!(!looks_permission_like("I prefer dark mode"));
    }
}
