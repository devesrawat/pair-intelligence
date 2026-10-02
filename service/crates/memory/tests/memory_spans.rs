//! Evidence span verification: when the source text is supplied at propose time, a span that is
//! not in it is rejected; evidence without supplied text is flagged `span_verified = false`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, source, TestDb};
use pair_core::{error::ErrorCode, traits::Memory, types::TrustClass};
use pair_memory::{inbox::EditPatch, CandidateDraft};

const NOTE: &str = "Standup notes.\nThe dog is\n  named   Rex and lives in Jaipur.";

#[tokio::test]
async fn span_not_in_source_text_rejected() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "note", TrustClass::Owner).await;

    let bad = CandidateDraft::new(candidate(
        "fact",
        "The dog is named Tom",
        None,
        src.id,
        "dog named Tom",
    ))
    .with_source_text(src.id, NOTE);
    let err = mem.propose_with_outcome(bad).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_candidates")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "rejected candidates are not stored");

    // Whitespace differences (line wraps, indentation) do not matter; the words must match.
    let good = CandidateDraft::new(candidate(
        "fact",
        "The dog is named Rex",
        None,
        src.id,
        "dog is named Rex",
    ))
    .with_source_text(src.id, NOTE);
    let proposal = mem.propose_with_outcome(good).await.unwrap();
    let entry = mem.get_candidate(proposal.id).await.unwrap();
    assert!(entry.evidence[0].span_verified);
}

#[tokio::test]
async fn sources_without_text_are_flagged_unverified_in_export() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let with_text = source(&mem, "has-text", TrustClass::Owner).await;
    let without = source(&mem, "no-text", TrustClass::Owner).await;

    let a = CandidateDraft::new(candidate(
        "fact",
        "The dog is named Rex",
        None,
        with_text.id,
        "dog is named Rex",
    ))
    .with_source_text(with_text.id, NOTE);
    let a = mem.propose_draft(a).await.unwrap();
    mem.accept(a, "owner").await.unwrap();
    let b = mem
        .propose(candidate(
            "fact",
            "The cat is named Tom",
            None,
            without.id,
            "cat named Tom",
        ))
        .await
        .unwrap();
    mem.accept(b, "owner").await.unwrap();

    let export = mem.export_json().await.unwrap();
    let flag = |needle: &str| {
        export["memories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["content"].as_str().unwrap().contains(needle))
            .unwrap()["evidence"][0]["span_verified"]
            .as_bool()
            .unwrap()
    };
    assert!(flag("Rex"));
    assert!(!flag("Tom"));
    assert!(mem.export_markdown().await.unwrap().contains("unverified"));
}

#[tokio::test]
async fn edit_keeps_span_verification_of_original_evidence() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "note", TrustClass::Owner).await;
    let d = CandidateDraft::new(candidate(
        "fact",
        "The dog is named Rex",
        None,
        src.id,
        "dog is named Rex",
    ))
    .with_source_text(src.id, NOTE);
    let id = mem.propose_draft(d).await.unwrap();
    let edited = mem
        .edit_candidate(
            id,
            "owner",
            EditPatch {
                content: Some("The dog is named Rex and lives in Jaipur".into()),
                ..EditPatch::default()
            },
        )
        .await
        .unwrap();
    let entry = mem.get_candidate(edited).await.unwrap();
    assert!(entry.evidence[0].span_verified);
    let memory = mem.accept(edited, "owner").await.unwrap();
    assert!(mem.get_memory(memory).await.unwrap().evidence[0].span_verified);
}
