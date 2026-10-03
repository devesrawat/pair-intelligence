#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::support::statement_supported;
use super::*;
use chrono::Utc;
use pair_core::ids::SourceId;
use uuid::Uuid;

const KNOWN_HEADINGS: [&str; 7] = [
    "# Research report",
    "## Findings",
    "## Conflicting evidence",
    "## Evidence",
    "## Sources",
    "## Rejected (not used)",
    "## Limitations",
];

fn source(url: &str) -> Source {
    Source {
        id: SourceId::new(),
        url: url.into(),
        normalized_url: url.into(),
        available: true,
        unavailable_reason: None,
        revision: None,
        content_sha256: Some("ab".repeat(32)),
        published_at: None,
        fetched_at: Utc::now(),
        text: Some("text".into()),
        duplicate_of: None,
    }
}

fn claim(src: &Source, text: &str) -> Claim {
    Claim {
        id: Uuid::now_v7(),
        raw: RawClaim {
            topic: "topic".into(),
            text: text.into(),
            value: "v".into(),
            url: src.url.clone(),
            span: "some supporting span".into(),
        },
        source: Some(src.id),
        span_start: Some(0),
        rejected: None,
    }
}

fn report(sources: Vec<Source>, claims: Vec<Claim>, statements: Vec<Statement>) -> Report {
    Report {
        run_id: Uuid::now_v7(),
        question: "q".into(),
        generated_at: Utc::now(),
        sources,
        claims,
        conflicts: Vec::new(),
        statements,
        rejected_statements: Vec::new(),
        limitations: Vec::new(),
    }
}

/// The report with every code span removed (what a renderer treats as live markup).
fn outside_code(md: &str) -> String {
    md.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 0)
        .map(|(_, p)| p)
        .collect::<Vec<_>>()
        .join("")
}

fn unescaped(md: &str, ch: char) -> bool {
    let chars: Vec<char> = md.chars().collect();
    chars
        .iter()
        .enumerate()
        .any(|(i, c)| *c == ch && (i == 0 || chars[i - 1] != '\\'))
}

fn headings(md: &str) -> Vec<&str> {
    md.lines()
        .filter(|l| l.trim_start().starts_with('#'))
        .collect()
}

#[test]
fn report_url_with_newline_cannot_inject_heading() {
    let evil = "https://example.com/a\n\n# Injected heading\n[click](https://evil.test)";
    let src = source(evil);
    let c = claim(&src, "plain claim");
    let md = render_markdown(&report(vec![src], vec![c], Vec::new()));
    for h in headings(&md) {
        assert!(KNOWN_HEADINGS.contains(&h), "injected heading line: {h:?}");
    }
    assert!(!md.contains("\n# Injected"));
    let live = outside_code(&md);
    assert!(!live.contains("]("), "{live}");
    // the URL only ever appears inside a code span, on a single line
    assert!(md.contains("`https://example.com/a"));
    assert!(md.contains("%0A"));
}

#[test]
fn report_image_markdown_neutralised() {
    let src = source("https://example.com/p");
    let beacon =
        "![x](https://evil.test/beacon.png?q=1) see https://evil.test/x [a](b) <img src=x>";
    let c = claim(&src, beacon);
    let st = Statement {
        text: beacon.into(),
        claim_ids: vec![c.id],
    };
    let md = render_markdown(&report(vec![src], vec![c], vec![st]));
    let live = outside_code(&md);
    // generated text has no '!' or '<'; any live one would be page-derived
    for ch in ['!', '<'] {
        assert!(!unescaped(&live, ch), "unescaped {ch:?} in {live}");
    }
    assert!(!live.contains("!["), "{live}");
    assert!(!live.contains("]("), "{live}");
    assert!(
        !md.contains("https://evil.test"),
        "no raw URL outside code spans"
    );
}

#[test]
fn statement_negation_flip_rejected() {
    let spans = ["The drug is safe for adults"];
    assert!(statement_supported("The drug is safe for adults", &spans));
    assert!(!statement_supported(
        "The drug is not safe for adults",
        &spans
    ));
    // an even number of negations on both sides still agrees
    let neg = ["The drug is not safe and never approved for adults"];
    assert!(statement_supported(
        "The drug is not safe and never approved for adults",
        &neg
    ));
}
