//! Evidence span verification against caller-supplied source text.

/// Collapse every whitespace run to a single space so line wraps and indentation in the source
/// do not defeat an otherwise exact quote. Case and wording must match exactly.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when `span` (ignoring whitespace layout) occurs verbatim in `source_text`.
pub fn span_in_text(span: &str, source_text: &str) -> bool {
    let needle = squash(span);
    !needle.is_empty() && squash(source_text).contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::span_in_text;

    #[test]
    fn test_span_in_text_whitespace_layout_ignored() {
        assert!(span_in_text(
            "dog is named Rex",
            "The dog is\n  named   Rex."
        ));
    }

    #[test]
    fn test_span_in_text_wrong_case_rejected() {
        assert!(!span_in_text("dog is named rex", "The dog is named Rex."));
    }

    #[test]
    fn test_span_in_text_blank_span_never_verifies() {
        assert!(!span_in_text("   ", "anything"));
    }
}
