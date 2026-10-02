//! Retrieval evaluation on the fixture corpus (`evals/datasets/memory_corpus.jsonl`) with the
//! query set `evals/datasets/memory.jsonl`. Run via `scripts/evaluate-memory`.
//!
//! These are fixture-corpus numbers. They show regressions and relative changes only; they say
//! nothing about recall on real usage.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::TestDb;
use pair_core::{
    ids::{MemoryId, SourceId},
    types::{EvidenceRef, MemoryCandidate, RetrievalQuery, TrustClass},
};
use pair_memory::{CandidateDraft, NewSource, PgMemory};
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf};

#[derive(Deserialize)]
struct CorpusRec {
    key: String,
    kind: String,
    content: String,
    project: Option<String>,
    source: String,
    span: String,
    topic: String,
    supersedes: Option<String>,
    inferred: bool,
}

#[derive(Deserialize)]
struct QueryRec {
    id: String,
    query: String,
    project: Option<String>,
    expect_memory: Option<String>,
    expect_source: Option<String>,
    held_out: bool,
}

fn dataset<T: for<'de> Deserialize<'de>>(name: &str) -> Vec<T> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../evals/datasets")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

async fn seed(
    mem: &PgMemory,
    corpus: &[CorpusRec],
) -> (HashMap<String, MemoryId>, HashMap<String, SourceId>) {
    let mut sources: HashMap<String, SourceId> = HashMap::new();
    let mut memories: HashMap<String, MemoryId> = HashMap::new();
    for rec in corpus {
        if !sources.contains_key(&rec.source) {
            let s = mem
                .register_source(NewSource::new(
                    "doc",
                    rec.source.clone(),
                    format!("hash-{}", rec.source),
                    TrustClass::Owner,
                ))
                .await
                .unwrap();
            sources.insert(rec.source.clone(), s.id);
        }
        let mut draft = CandidateDraft::new(MemoryCandidate {
            kind: rec.kind.clone(),
            content: rec.content.clone(),
            project: rec.project.clone(),
            inferred: rec.inferred,
            evidence: vec![EvidenceRef {
                source: sources[&rec.source],
                span: Some(rec.span.clone()),
            }],
        });
        draft.topic = Some(rec.topic.clone());
        draft.reason = Some("fixture corpus".into());
        let proposal = mem.propose_with_outcome(draft).await.unwrap();
        let id = match (proposal.auto_accepted, &rec.supersedes) {
            (Some(id), _) => id,
            (None, Some(old)) => mem
                .accept_superseding(proposal.id, "eval", memories[old])
                .await
                .unwrap(),
            (None, None) => pair_core::traits::Memory::accept(mem, proposal.id, "eval")
                .await
                .unwrap(),
        };
        memories.insert(rec.key.clone(), id);
    }
    (memories, sources)
}

#[derive(Default, Clone, Copy)]
struct Tally {
    answerable: u32,
    found: u32,
    top1: u32,
    unknown: u32,
    unsupported: u32,
}

impl Tally {
    fn recall(&self) -> f64 {
        f64::from(self.found) / f64::from(self.answerable.max(1))
    }
    fn top1_rate(&self) -> f64 {
        f64::from(self.top1) / f64::from(self.answerable.max(1))
    }
    fn unsupported_rate(&self) -> f64 {
        f64::from(self.unsupported) / f64::from(self.unknown.max(1))
    }
    fn line(&self, label: &str) -> String {
        format!(
            "{label:<10} answerable={:>2} recall@8={:.3} top1={:.3} | unknown={:>2} unsupported_answer_rate={:.3}",
            self.answerable,
            self.recall(),
            self.top1_rate(),
            self.unknown,
            self.unsupported_rate()
        )
    }
}

#[tokio::test]
async fn evaluate_memory_retrieval() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let corpus: Vec<CorpusRec> = dataset("memory_corpus.jsonl");
    let queries: Vec<QueryRec> = dataset("memory.jsonl");
    assert!(queries.len() >= 50, "dataset must hold at least 50 queries");
    let (memories, sources) = seed(&mem, &corpus).await;

    let (mut dev, mut held) = (Tally::default(), Tally::default());
    let mut misses = Vec::new();
    for q in &queries {
        let hits = mem
            .retrieve_detailed(RetrievalQuery {
                text: q.query.clone(),
                project: q.project.clone(),
                as_of: chrono::Utc::now(),
                limit: 0,
            })
            .await
            .unwrap();
        let tally = if q.held_out { &mut held } else { &mut dev };
        match (&q.expect_memory, &q.expect_source) {
            (Some(key), Some(src)) => {
                tally.answerable += 1;
                let (want, want_src) = (memories[key], sources[src]);
                let position = hits.iter().position(|h| {
                    h.item.memory == want
                        && h.current
                        && h.item
                            .evidence
                            .iter()
                            .any(|e| e.source == want_src && e.span.is_some())
                });
                match position {
                    Some(p) => {
                        tally.found += 1;
                        tally.top1 += u32::from(p == 0);
                    }
                    None => misses.push(format!(
                        "MISS {} {:?} got {} items",
                        q.id,
                        q.query,
                        hits.len()
                    )),
                }
            }
            _ => {
                tally.unknown += 1;
                if !hits.is_empty() {
                    tally.unsupported += 1;
                    misses.push(format!(
                        "UNSUPPORTED {} {:?} returned {:?}",
                        q.id,
                        q.query,
                        hits.iter()
                            .map(|h| h.item.content.clone())
                            .collect::<Vec<_>>()
                    ));
                }
            }
        }
    }
    let all = Tally {
        answerable: dev.answerable + held.answerable,
        found: dev.found + held.found,
        top1: dev.top1 + held.top1,
        unknown: dev.unknown + held.unknown,
        unsupported: dev.unsupported + held.unsupported,
    };
    println!(
        "\nFIXTURE-CORPUS NUMBERS (not real usage); corpus={} memories, queries={}",
        corpus.len(),
        queries.len()
    );
    println!("{}", dev.line("dev"));
    println!("{}", held.line("held_out"));
    println!("{}", all.line("all"));
    for m in &misses {
        println!("{m}");
    }
    println!(
        "EVAL_JSON {}",
        serde_json::json!({
            "corpus": "fixture", "queries": queries.len(),
            "held_out": {"recall_at_8": held.recall(), "top1": held.top1_rate(), "unsupported_answer_rate": held.unsupported_rate(), "answerable": held.answerable, "unknown": held.unknown},
            "dev": {"recall_at_8": dev.recall(), "top1": dev.top1_rate(), "unsupported_answer_rate": dev.unsupported_rate(), "answerable": dev.answerable, "unknown": dev.unknown},
            "all": {"recall_at_8": all.recall(), "top1": all.top1_rate(), "unsupported_answer_rate": all.unsupported_rate()},
        })
    );

    // Regression floors on the fixture corpus.
    assert!(
        held.answerable >= 10 && held.unknown >= 3,
        "held-out split must cover both query types"
    );
    assert!(
        all.recall() >= RECALL_FLOOR,
        "recall regressed: {:.3}",
        all.recall()
    );
    assert!(
        all.unsupported_rate() <= UNSUPPORTED_CEILING,
        "unsupported answers regressed: {:.3}",
        all.unsupported_rate()
    );
}

const RECALL_FLOOR: f64 = 0.9;
const UNSUPPORTED_CEILING: f64 = 0.1;
