//! Deterministic citation support check over captured text, plus the LLM-judge hook.
//!
//! A citation is valid only if (1) the span really occurs in the captured source text,
//! (2) every figure and the asserted value in the claim occur in the span, (3) enough of
//! the claim's content words occur in the span, and (4) the claim does not flip a negation.
use super::types::RawClaim;
use async_trait::async_trait;
use pair_core::error::Result;
use std::collections::HashSet;

const MIN_SPAN_CHARS: usize = 12;
const MIN_COVERAGE: f64 = 0.6;
/// Looser for synthesized statements (connective wording); figures stay strict.
const MIN_STATEMENT_COVERAGE: f64 = 0.4;
const STOPWORDS: [&str; 24] = [
    "the", "a", "an", "of", "in", "on", "at", "to", "is", "are", "was", "were", "and", "or", "for", "by", "with",
    "that", "this", "it", "its", "as", "be", "from",
];
const NEGATIONS: [&str; 6] = ["not", "no", "never", "without", "cannot", "nor"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Supported { span_start: usize },
    SpanNotInSource,
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeVerdict {
    Supports,
    DoesNotSupport(String),
}

/// Optional semantic check (typically an LLM). It can only veto: the deterministic
/// check is always required. Implementations must treat all inputs as untrusted data.
#[async_trait]
pub trait SupportJudge: Send + Sync {
    async fn judge(&self, claim_text: &str, span: &str, source_url: &str) -> Result<JudgeVerdict>;
}

fn fold(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' => '\'',
        '\u{201C}' | '\u{201D}' => '"',
        '\u{2013}' | '\u{2014}' => '-',
        other => other,
    }
}

/// Lowercased, whitespace-collapsed chars with a map back to original char offsets.
fn normalize_with_map(text: &str) -> (Vec<char>, Vec<usize>) {
    let (mut out, mut map) = (Vec::new(), Vec::new());
    let mut last_space = true;
    for (i, c) in text.chars().enumerate() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                map.push(i);
            }
            last_space = true;
        } else {
            for lc in fold(c).to_lowercase() {
                out.push(lc);
                map.push(i);
            }
            last_space = false;
        }
    }
    while out.last() == Some(&' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

/// Char offset in `source` of the first whitespace/case-insensitive occurrence of `span`.
pub fn locate_span(source: &str, span: &str) -> Option<usize> {
    let (hay, map) = normalize_with_map(source);
    let (needle, _) = normalize_with_map(span);
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle.as_slice()).map(|p| map[p])
}

fn tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().map(fold).collect();
    let digit_at = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|c| c.is_ascii_digit());
    let letter_at = |i: Option<usize>| i.and_then(|i| chars.get(i)).is_some_and(|c| c.is_alphabetic());
    let (mut out, mut cur) = (Vec::new(), String::new());
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1);
        let keep = c.is_alphanumeric()
            || (matches!(c, '.' | ',') && digit_at(prev) && digit_at(Some(i + 1)))
            || (c == '%' && digit_at(prev))
            || (c == '\'' && letter_at(prev) && letter_at(Some(i + 1)));
        if keep {
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|t| t.replace(',', "")).collect()
}

fn stem(t: &str) -> String {
    if t.len() > 3 && t.ends_with('s') && !t.ends_with("ss") && !t.chars().any(|c| c.is_ascii_digit()) {
        t[..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn is_numeric(t: &str) -> bool {
    t.chars().any(|c| c.is_ascii_digit())
}

fn negations(toks: &[String]) -> usize {
    toks.iter().filter(|t| NEGATIONS.contains(&t.as_str()) || t.ends_with("n't")).count()
}

/// Deterministic support decision for one claim against one captured source text.
pub fn check_support(claim: &RawClaim, source_text: &str) -> Support {
    let span = claim.span.trim();
    if span.chars().count() < MIN_SPAN_CHARS {
        return Support::Unsupported("supporting span is too short to be meaningful".into());
    }
    let Some(span_start) = locate_span(source_text, span) else {
        return Support::SpanNotInSource;
    };
    let span_toks = tokens(span);
    let span_set: HashSet<String> = span_toks.iter().map(|t| stem(t)).collect();
    let claim_toks = tokens(&claim.text);

    if let Some(n) = claim_toks.iter().chain(&tokens(&claim.value)).find(|t| is_numeric(t) && !span_set.contains(*t)) {
        return Support::Unsupported(format!("figure {n:?} does not appear in the cited span"));
    }
    let value_toks = tokens(&claim.value);
    if value_toks.is_empty() || !value_toks.iter().all(|t| span_set.contains(&stem(t))) {
        return Support::Unsupported("asserted value does not appear in the cited span".into());
    }
    let content: Vec<String> =
        claim_toks.iter().filter(|t| !STOPWORDS.contains(&t.as_str())).map(|t| stem(t)).collect();
    if !content.is_empty() {
        let hit = content.iter().filter(|t| span_set.contains(*t)).count();
        let coverage = hit as f64 / content.len() as f64;
        if coverage < MIN_COVERAGE {
            return Support::Unsupported(format!("only {:.0}% of the claim's terms appear in the span", coverage * 100.0));
        }
    }
    if negations(&claim_toks) % 2 != negations(&span_toks) % 2 {
        return Support::Unsupported("claim and span disagree on negation".into());
    }
    Support::Supported { span_start }
}

/// Supported-statement check: statement figures and most terms must occur in the cited spans.
pub fn statement_supported(statement: &str, spans: &[&str]) -> bool {
    let union: HashSet<String> = spans.iter().flat_map(|s| tokens(s)).map(|t| stem(&t)).collect();
    let toks = tokens(statement);
    if toks.iter().any(|t| is_numeric(t) && !union.contains(t)) {
        return false;
    }
    let content: Vec<String> = toks.iter().filter(|t| !STOPWORDS.contains(&t.as_str())).map(|t| stem(t)).collect();
    if content.is_empty() {
        return false;
    }
    let hit = content.iter().filter(|t| union.contains(*t)).count();
    hit as f64 / content.len() as f64 >= MIN_STATEMENT_COVERAGE
}
