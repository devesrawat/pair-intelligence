//! Markdown export. Page-derived strings are flattened to one line so captured text
//! cannot inject headings or links into the report structure.
use super::types::{Claim, Report, Source};
use std::fmt::Write;

fn inline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn source_label(sources: &[Source], c: &Claim) -> String {
    let src = c.source.and_then(|id| sources.iter().find(|s| s.id == id));
    match src {
        Some(s) => {
            let date = s.published_at.map_or("date unknown".to_string(), |d| d.to_string());
            let hash = s.content_sha256.as_deref().map_or(String::new(), |h| format!(", sha256 {}", &h[..h.len().min(12)]));
            format!("{} ({date}{hash})", s.url)
        }
        None => c.raw.url.clone(),
    }
}

pub fn render_markdown(r: &Report) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# Research report\n\n**Question:** {}\n\nGenerated {} (run {}).\n", inline(&r.question), r.generated_at.to_rfc3339(), r.run_id);

    let _ = writeln!(o, "## Findings\n");
    if r.statements.is_empty() {
        let _ = writeln!(o, "_No statement passed citation validation._\n");
    }
    for st in &r.statements {
        let refs: Vec<String> = st.claim_ids.iter().map(|id| format!("[{}]", &id.simple().to_string()[..8])).collect();
        let _ = writeln!(o, "- {} {}", inline(&st.text), refs.join(""));
    }

    let _ = writeln!(o, "\n## Conflicting evidence\n");
    if r.conflicts.is_empty() {
        let _ = writeln!(o, "_None detected._");
    }
    for k in &r.conflicts {
        let _ = writeln!(o, "- **{}** — sources disagree:", inline(&k.topic));
        for (value, ids) in &k.positions {
            for c in r.claims.iter().filter(|c| ids.contains(&c.id)) {
                let _ = writeln!(o, "  - `{}`: {} — {}", inline(value), inline(&c.raw.text), source_label(&r.sources, c));
            }
        }
    }

    let _ = writeln!(o, "\n## Evidence\n");
    for c in r.claims.iter().filter(|c| c.is_valid()) {
        let _ = writeln!(o, "- [{}] {}\n  > {}\n  > {}", &c.id.simple().to_string()[..8], inline(&c.raw.text), inline(&c.raw.span), source_label(&r.sources, c));
    }

    let _ = writeln!(o, "\n## Sources\n");
    for s in &r.sources {
        match (&s.unavailable_reason, s.duplicate_of) {
            (Some(why), _) => {
                let _ = writeln!(o, "- {} — **UNAVAILABLE**: {}", s.url, inline(why));
            }
            (None, Some(_)) => {
                let _ = writeln!(o, "- {} — duplicate of an earlier source", s.url);
            }
            (None, None) => {
                let date = s.published_at.map_or("date unknown".to_string(), |d| d.to_string());
                let _ = writeln!(o, "- {} — {date}, fetched {}", s.url, s.fetched_at.to_rfc3339());
            }
        }
    }

    let rejected: Vec<&Claim> = r.claims.iter().filter(|c| !c.is_valid()).collect();
    if !rejected.is_empty() || !r.rejected_statements.is_empty() {
        let _ = writeln!(o, "\n## Rejected (not used)\n");
        for c in rejected {
            let why = c.rejected.as_ref().map(ToString::to_string).unwrap_or_default();
            let _ = writeln!(o, "- claim \"{}\" ({}): {why}", inline(&c.raw.text), inline(&c.raw.url));
        }
        for s in &r.rejected_statements {
            let _ = writeln!(o, "- statement \"{}\": {}", inline(&s.text), inline(&s.reason));
        }
    }

    let _ = writeln!(o, "\n## Limitations\n");
    for l in &r.limitations {
        let _ = writeln!(o, "- {l}");
    }
    o
}
