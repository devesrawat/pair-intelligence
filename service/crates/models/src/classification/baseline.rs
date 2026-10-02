//! Deterministic rules and the fixed baseline. No network, no model.
//! Rules are a coarse keyword heuristic used to (a) detect exact commands, (b) fill the
//! `TaskProfile` that the baseline router needs, and (c) serve as the "deterministic rules" eval strategy.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleOutcome {
    pub intent: &'static str,
    pub difficulty: &'static str,
    /// Exact deterministic command (e.g. `/status`): execute its handler, skip classification.
    pub exact_command: bool,
}

const EXACT_COMMANDS: [&str; 5] = ["/status", "/budget", "/help", "/jobs", "/memory"];
const LONG_REQUEST_CHARS: usize = 600;

const INTENT_KEYWORDS: [(&str, &[&str]); 5] = [
    ("coding", &[
        "fix", "bug", "refactor", "function", "compile", "cargo", "rust", "commit", "stack trace", "implement",
        "endpoint", "code", "repo", "migration", "unit test", "failing test", "clippy", "build fail", "pr ", "api ",
        "banao code", "error aa",
    ]),
    ("research", &[
        "research", "compare", "sources", "paper", "latest", "survey", "benchmark", "citation", "literature",
        "vs ", "pricing", "market", "kaun sa behtar",
    ]),
    ("planning", &[
        "plan", "schedule", "roadmap", "prioriti", "tomorrow", "agenda", "goals", "this week", "deadline",
        "kal ", "aaj ", "todo", "milestone",
    ]),
    ("memory_recall", &[
        "remember", "what did i", "last time", "recall", "did i tell", "earlier", "we agreed", "i decided",
        "pichli baar", "kya maine", "what was the", "remind me what",
    ]),
    ("transformation", &[
        "translate", "rewrite", "reformat", "convert", "summarize this", "proofread", "shorten", "paraphrase",
        "turn this into", "anuvad", "badlo",
    ]),
];
const DEEP_CUES: [&str; 8] = [
    "architecture", "design a ", "from scratch", "end to end", "security audit", "race condition", "trade-off", "tradeoff",
];
const ROUTINE_CUES: [&str; 8] = ["typo", "rename", "format", "one line", "simple", "quick", "tiny", "spelling"];

/// Rule-based intent and difficulty. Ties between two or more intents yield `mixed`; no match yields `uncertain`.
pub fn classify_by_rules(text: &str) -> RuleOutcome {
    let lower = format!("{} ", text.trim().to_lowercase());
    let trimmed = lower.trim();
    if EXACT_COMMANDS.contains(&trimmed) {
        return RuleOutcome { intent: "uncertain", difficulty: "routine", exact_command: true };
    }
    let scores: Vec<(&'static str, usize)> = INTENT_KEYWORDS
        .iter()
        .map(|(intent, kws)| (*intent, kws.iter().filter(|k| lower.contains(*k)).count()))
        .collect();
    let top = scores.iter().map(|s| s.1).max().unwrap_or(0);
    let leaders: Vec<&'static str> = scores.iter().filter(|s| s.1 == top && top > 0).map(|s| s.0).collect();
    let intent = match leaders.as_slice() {
        [] => "uncertain",
        [one] => one,
        _ => "mixed",
    };
    let difficulty = if DEEP_CUES.iter().any(|c| lower.contains(c)) {
        "deep"
    } else if intent == "uncertain" {
        "uncertain"
    } else if ROUTINE_CUES.iter().any(|c| lower.contains(c)) {
        "routine"
    } else if lower.chars().count() > LONG_REQUEST_CHARS {
        "substantial"
    } else {
        match intent {
            "transformation" | "memory_recall" => "routine",
            _ => "substantial",
        }
    };
    RuleOutcome { intent, difficulty, exact_command: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_by_rules_exact_command_flagged() {
        assert!(classify_by_rules("/status").exact_command);
        assert!(!classify_by_rules("what is the status of my PR").exact_command);
    }

    #[test]
    fn test_classify_by_rules_single_intent() {
        assert_eq!(classify_by_rules("fix the failing test in budget").intent, "coding");
        assert_eq!(classify_by_rules("translate this email to Hindi").intent, "transformation");
    }

    #[test]
    fn test_classify_by_rules_no_match_is_uncertain() {
        let out = classify_by_rules("hmm");
        assert_eq!((out.intent, out.difficulty), ("uncertain", "uncertain"));
    }

    #[test]
    fn test_classify_by_rules_misleading_complexity_cue_is_a_known_weakness() {
        // "architecture" forces deep even on a trivial request: documented rule limitation measured by evals.
        assert_eq!(classify_by_rules("fix typo in architecture.md").difficulty, "deep");
    }
}
