//! Markdown export. Everything derived from a page or a model is rendered inert:
//! URLs only inside code spans (newlines percent-encoded), other strings flattened to one
//! line with markdown metacharacters backslash-escaped and bare URLs defanged, so captured
//! text cannot inject headings, links, images or autolinks into the report.
use super::types::{Claim, Report, Source};
use std::fmt::Write;

const ZERO_WIDTH_SPACE: char = '\u{200B}';
/// Characters that carry markdown or HTML meaning. The backslash comes first in intent:
/// every occurrence is escaped exactly once, in a single pass.
const MD_SPECIAL: &str = "\\[]()!<>`#*_|~&";

fn flatten(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Page- or model-derived prose: one line, metacharacters escaped, URLs defanged.
fn esc(s: &str) -> String {
    let flat = flatten(s)
        .replace("://", &format!(":{ZERO_WIDTH_SPACE}//"))
        .replace("www.", &format!("www.{ZERO_WIDTH_SPACE}"));
    let mut out = String::with_capacity(flat.len());
    for c in flat.chars() {
        if MD_SPECIAL.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Literal text in a code span. Whitespace/control characters and backticks are
/// percent-encoded so the span cannot end early or span lines.
fn code(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('`');
    for c in s.trim().chars() {
        if c == '`' || c.is_control() || c.is_whitespace() {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        } else {
            out.push(c);
        }
    }
    out.push('`');
    out
}

fn source_label(sources: &[Source], c: &Claim) -> String {
    let src = c.source.and_then(|id| sources.iter().find(|s| s.id == id));
    match src {
        Some(s) => {
            let date = s
                .published_at
                .map_or("date unknown".to_string(), |d| d.to_string());
            let hash = s.content_sha256.as_deref().map_or(String::new(), |h| {
                format!(", sha256 {}", &h[..h.len().min(12)])
            });
            format!("{} ({date}{hash})", code(&s.url))
        }
        None => code(&c.raw.url),
    }
}

pub fn render_markdown(r: &Report) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "# Research report\n\n**Question:** {}\n\nGenerated {} (run {}).\n",
        esc(&r.question),
        r.generated_at.to_rfc3339(),
        r.run_id
    );

    let _ = writeln!(o, "## Findings\n");
    if r.statements.is_empty() {
        let _ = writeln!(o, "_No statement passed citation validation._\n");
    }
    for st in &r.statements {
        let refs: Vec<String> = st
            .claim_ids
            .iter()
            .map(|id| format!("[{}]", &id.simple().to_string()[..8]))
            .collect();
        let _ = writeln!(o, "- {} {}", esc(&st.text), refs.join(""));
    }

    let _ = writeln!(o, "\n## Conflicting evidence\n");
    if r.conflicts.is_empty() {
        let _ = writeln!(o, "_None detected._");
    }
    for k in &r.conflicts {
        let _ = writeln!(o, "- **{}** — sources disagree:", esc(&k.topic));
        for (value, ids) in &k.positions {
            for c in r.claims.iter().filter(|c| ids.contains(&c.id)) {
                let _ = writeln!(
                    o,
                    "  - {}: {} — {}",
                    code(value),
                    esc(&c.raw.text),
                    source_label(&r.sources, c)
                );
            }
        }
    }

    let _ = writeln!(o, "\n## Evidence\n");
    for c in r.claims.iter().filter(|c| c.is_valid()) {
        let _ = writeln!(
            o,
            "- [{}] {}\n  > {}\n  > {}",
            &c.id.simple().to_string()[..8],
            esc(&c.raw.text),
            esc(&c.raw.span),
            source_label(&r.sources, c)
        );
    }

    let _ = writeln!(o, "\n## Sources\n");
    for s in &r.sources {
        match (&s.unavailable_reason, s.duplicate_of) {
            (Some(why), _) => {
                let _ = writeln!(o, "- {} — **UNAVAILABLE**: {}", code(&s.url), esc(why));
            }
            (None, Some(_)) => {
                let _ = writeln!(o, "- {} — duplicate of an earlier source", code(&s.url));
            }
            (None, None) => {
                let date = s
                    .published_at
                    .map_or("date unknown".to_string(), |d| d.to_string());
                let _ = writeln!(
                    o,
                    "- {} — {date}, fetched {}",
                    code(&s.url),
                    s.fetched_at.to_rfc3339()
                );
            }
        }
    }

    let rejected: Vec<&Claim> = r.claims.iter().filter(|c| !c.is_valid()).collect();
    if !rejected.is_empty() || !r.rejected_statements.is_empty() {
        let _ = writeln!(o, "\n## Rejected (not used)\n");
        for c in rejected {
            let why = c
                .rejected
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            let _ = writeln!(
                o,
                "- claim \"{}\" ({}): {}",
                esc(&c.raw.text),
                code(&c.raw.url),
                esc(&why)
            );
        }
        for s in &r.rejected_statements {
            let _ = writeln!(o, "- statement \"{}\": {}", esc(&s.text), esc(&s.reason));
        }
    }

    let _ = writeln!(o, "\n## Limitations\n");
    for l in &r.limitations {
        let _ = writeln!(o, "- {l}");
    }
    o
}
