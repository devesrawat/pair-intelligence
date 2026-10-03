//! Review flows: edits, dedupe identity, supersession scope and corrections.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, source, verified, TestDb};
use pair_core::types::{EvidenceRef, MemoryCandidate};
use pair_core::{error::ErrorCode, ids::SourceId, traits::Memory, types::TrustClass};
use pair_memory::{inbox::EditPatch, CandidateDraft};

fn draft(
    kind: &str,
    content: &str,
    project: Option<&str>,
    src: SourceId,
    span: &str,
) -> CandidateDraft {
    verified(candidate(kind, content, project, src, span))
}

#[tokio::test]
async fn candidate_without_evidence_is_rejected_at_propose() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let err = mem
        .propose_with_outcome(CandidateDraft::new(MemoryCandidate {
            kind: "fact".into(),
            content: "Nobody said this".into(),
            project: None,
            inferred: false,
            evidence: Vec::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::MemoryNoEvidence);
}

#[tokio::test]
async fn edit_of_candidate_whose_sources_were_deleted_is_refused() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let gone = source(&mem, "vault-note", TrustClass::Owner).await;
    let live = source(&mem, "other-note", TrustClass::Owner).await;
    let p = mem
        .propose_with_outcome(draft(
            "fact",
            "The vault hint is Bluebird",
            None,
            gone.id,
            "hint Bluebird",
        ))
        .await
        .unwrap();
    mem.delete_source(gone.id, "owner").await.unwrap();

    // Adding fresh evidence must not resurrect the deleted source's text.
    let err = mem
        .edit_candidate(
            p.id,
            "owner",
            EditPatch {
                extra_evidence: vec![EvidenceRef {
                    source: live.id,
                    span: Some("unrelated".into()),
                }],
                ..EditPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::SourceDeleted);
    assert!(!err.message.contains("Bluebird"));
    let candidates: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_candidates")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(candidates, 1, "no replacement candidate was created");
}

#[tokio::test]
async fn dedupe_identity_includes_kind_project_and_inferred() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "chat", TrustClass::Owner).await;
    let text = "Alpha owns the billing service";

    let base = mem
        .propose_with_outcome(draft(
            "fact",
            text,
            Some("alpha"),
            src.id,
            "alpha owns billing",
        ))
        .await
        .unwrap();
    let other_project = mem
        .propose_with_outcome(draft(
            "fact",
            text,
            Some("beta"),
            src.id,
            "alpha owns billing",
        ))
        .await
        .unwrap();
    let other_kind = mem
        .propose_with_outcome(draft(
            "decision",
            text,
            Some("alpha"),
            src.id,
            "alpha owns billing",
        ))
        .await
        .unwrap();
    let mut inferred = draft("fact", text, Some("alpha"), src.id, "alpha owns billing");
    inferred.candidate.inferred = true;
    let other_inferred = mem.propose_with_outcome(inferred).await.unwrap();
    let again = mem
        .propose_with_outcome(draft(
            "fact",
            text,
            Some("alpha"),
            src.id,
            "alpha owns billing",
        ))
        .await
        .unwrap();

    assert!(!other_project.duplicate && other_project.id != base.id);
    assert!(!other_kind.duplicate && other_kind.id != base.id);
    assert!(!other_inferred.duplicate && other_inferred.id != base.id);
    assert!(again.duplicate && again.id == base.id);
}

#[tokio::test]
async fn editing_only_kind_project_or_inferred_does_not_hit_the_unique_index() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "chat", TrustClass::Owner).await;

    for (i, patch) in [
        EditPatch {
            kind: Some("decision".into()),
            ..EditPatch::default()
        },
        EditPatch {
            project: Some("beta".into()),
            ..EditPatch::default()
        },
        EditPatch {
            inferred: Some(true),
            ..EditPatch::default()
        },
        EditPatch {
            reason: Some("new reason".into()),
            ..EditPatch::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let p = mem
            .propose_with_outcome(draft(
                "fact",
                &format!("Service {i} is owned by alpha"),
                Some("alpha"),
                src.id,
                "owned by alpha",
            ))
            .await
            .unwrap();
        let replacement = mem.edit_candidate(p.id, "owner", patch).await.unwrap();
        assert_ne!(replacement, p.id);
    }
}

#[tokio::test]
async fn restated_statement_does_not_collapse_onto_a_superseded_memory() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let s1 = source(&mem, "adr-1", TrustClass::Owner).await;
    let s2 = source(&mem, "adr-2", TrustClass::Owner).await;

    let first = mem
        .propose_with_outcome(draft(
            "decision",
            "Ledger storage: Postgres",
            None,
            s1.id,
            "Postgres",
        ))
        .await
        .unwrap();
    let old = mem.accept(first.id, "owner").await.unwrap();
    let second = mem
        .propose_with_outcome(draft(
            "decision",
            "Ledger storage: DynamoDB",
            None,
            s2.id,
            "DynamoDB",
        ))
        .await
        .unwrap();
    let current = mem
        .accept_superseding(second.id, "owner", old)
        .await
        .unwrap();

    let restated = mem
        .propose_with_outcome(draft(
            "decision",
            "Ledger storage: Postgres",
            None,
            s1.id,
            "Postgres",
        ))
        .await
        .unwrap();
    assert!(
        !restated.duplicate,
        "the old candidate's memory is superseded"
    );
    assert_ne!(restated.id, first.id);
    assert_eq!(restated.contradicts_memories, vec![current]);
    assert!(restated.needs_review);
}
