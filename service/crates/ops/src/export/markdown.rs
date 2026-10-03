//! Human-readable Markdown index. Every free-text value goes through [`indented_block`] or
//! [`code_span`], so stored content cannot start a heading, list item, fence or entry of its own.
//! Only database-generated values (uuids, enum labels, counts) appear outside those wrappers.
use super::{FileEntry, Row};
use std::fmt::Write;

/// Characters a Markdown renderer or line-based parser may treat as a line break.
const LINE_BREAKS: [char; 5] = ['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'];
const CODE_INDENT: &str = "    ";
const EMPTY: &str = "(none)";

/// Free text as an indented code block: every line is prefixed.
pub(super) fn indented_block(text: &str) -> String {
    let unified: String = text
        .chars()
        .map(|c| if LINE_BREAKS.contains(&c) { '\n' } else { c })
        .collect();
    unified
        .split('\n')
        .map(|line| format!("{CODE_INDENT}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Free text as a one-line code span: line breaks become a visible `\n`, and the delimiter is
/// longer than any backtick run inside, so the value cannot close the span.
pub(super) fn code_span(text: &str) -> String {
    let flat: String = text
        .chars()
        .flat_map(|c| {
            if LINE_BREAKS.contains(&c) {
                vec!['\\', 'n']
            } else {
                vec![c]
            }
        })
        .collect();
    let longest_run = flat
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest_run + 1);
    format!("{fence} {flat} {fence}")
}

fn span_of(v: &serde_json::Value, key: &str) -> String {
    v[key].as_str().map_or_else(|| EMPTY.to_owned(), code_span)
}

/// A value that must be a plain label (uuid, enum, timestamp) is shown only if it has no markup or
/// whitespace; anything else is wrapped like free text.
fn label(v: &serde_json::Value, key: &str) -> String {
    match v[key].as_str() {
        Some(s)
            if s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_:+.".contains(c)) =>
        {
            s.to_owned()
        }
        Some(s) => code_span(s),
        None => EMPTY.to_owned(),
    }
}

pub(super) struct IndexInput<'a> {
    pub files: &'a [FileEntry],
    pub conversations: &'a [Row],
    pub memories: &'a [Row],
    pub evidence: &'a [Row],
    pub goals: &'a [Row],
    pub open_loops: &'a [Row],
}

pub(super) fn render_index(input: &IndexInput<'_>) -> String {
    let mut out = String::from("# PAIR export\n\n");
    // Writing to a String cannot fail; results are intentionally discarded.
    out.push_str("## Files\n\n| file | rows | sha256 |\n|---|---|---|\n");
    for f in input.files {
        let _ = writeln!(out, "| {} | {} | {} |", f.name, f.rows, f.sha256);
    }
    let _ = writeln!(out, "\n## Conversations ({})\n", input.conversations.len());
    for c in input.conversations {
        let _ = writeln!(
            out,
            "- {} title {}",
            label(&c.value, "id"),
            span_of(&c.value, "title")
        );
    }
    let _ = writeln!(out, "\n## Memories ({})\n", input.memories.len());
    for m in input.memories {
        render_memory(&mut out, m, input.evidence);
    }
    let _ = writeln!(out, "## Goals ({})\n", input.goals.len());
    for g in input.goals {
        let _ = writeln!(
            out,
            "- {} [{}] {}",
            label(&g.value, "id"),
            label(&g.value, "status"),
            span_of(&g.value, "title")
        );
    }
    let _ = writeln!(out, "\n## Open loops ({})\n", input.open_loops.len());
    for l in input.open_loops {
        let _ = writeln!(
            out,
            "- {} [{}] {}",
            label(&l.value, "id"),
            label(&l.value, "status"),
            span_of(&l.value, "title")
        );
    }
    out
}

fn render_memory(out: &mut String, m: &Row, evidence: &[Row]) {
    let v = &m.value;
    let _ = writeln!(out, "### memory {}\n", label(v, "id"));
    let _ = writeln!(
        out,
        "- kind: {}\n- status: {}",
        label(v, "kind"),
        label(v, "status")
    );
    if v["tombstone"].as_bool() == Some(true) {
        let _ = writeln!(out, "- tombstone: source deleted, content not exported\n");
    } else {
        let _ = writeln!(out, "- project: {}\n", span_of(v, "project"));
        let _ = writeln!(
            out,
            "{}\n",
            indented_block(v["content"].as_str().unwrap_or_default())
        );
    }
    let id = v["id"].as_str().unwrap_or_default();
    for e in evidence
        .iter()
        .filter(|e| e.value["memory_id"].as_str() == Some(id))
    {
        let _ = writeln!(
            out,
            "- evidence: source {} kind {} span {}",
            label(&e.value, "source_id"),
            span_of(&e.value, "source_kind"),
            span_of(&e.value, "span"),
        );
    }
    out.push('\n');
}
