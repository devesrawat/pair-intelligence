//! Token counting behind a trait so real tokenizers can replace the estimate.

/// Characters per token used by the default estimator.
const CHARS_PER_TOKEN: u64 = 4;

pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> u64;
}

/// Deterministic estimate: `ceil(chars / 4)`. This is NOT provider accounting;
/// real usage is reconciled through the budget reserve.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApproxTokenCounter;

impl TokenCounter for ApproxTokenCounter {
    fn count(&self, text: &str) -> u64 {
        (text.chars().count() as u64).div_ceil(CHARS_PER_TOKEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_rounds_up_chars_over_four() {
        let c = ApproxTokenCounter;
        assert_eq!(c.count(""), 0);
        assert_eq!(c.count("a"), 1);
        assert_eq!(c.count("abcd"), 1);
        assert_eq!(c.count("abcde"), 2);
    }
}
