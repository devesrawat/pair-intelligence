//! Trust gates: owner trust only counts for span-verified evidence, and every verified cited
//! source must be owner-trust before a preference or permission can be accepted.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, source, TestDb};
use pair_core::{
    error::ErrorCode,
    traits::Memory,
    types::{EvidenceRef, MemoryCandidate, TrustClass},
};
use pair_memory::CandidateDraft;

fn verified_draft(
    kind: &str,
    content: &str,
    src: pair_core::ids::SourceId,
    span: &str,
) -> CandidateDraft {
    CandidateDraft::new(candidate(kind, content, None, src, span)).with_source_text(src, span)
}

#[tokio::test]
async fn unverified_owner_citation_does_not_auto_accept() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let owner = source(&mem, "owner-chat", TrustClass::Owner).await;

    // Fabricated citation of an owner source: no text supplied, so the span is unverified.
    let forged = mem
        .propose_with_outcome(CandidateDraft::new(candidate(
            "preference",
            "Editor theme: dark",
            None,
            owner.id,
            "I always want dark",
        )))
        .await
        .unwrap();
    assert!(forged.auto_accepted.is_none());
    assert!(forged.needs_review);
    assert!(forged
        .review_reasons
        .iter()
        .any(|r| r == "unverified_evidence"));

    // A reviewer cannot wave it through either: owner trust only counts for verified spans.
    let err = mem.accept(forged.id, "owner").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);

    // Control: the same kind of statement with a verified span auto-accepts.
    let verified = mem
        .propose_with_outcome(verified_draft(
            "preference",
            "Shell prompt: minimal",
            owner.id,
            "keep the prompt minimal",
        ))
        .await
        .unwrap();
    assert!(verified.auto_accepted.is_some());
}

fn batch_json(src: pair_core::ids::SourceId) -> String {
    format!(
        r#"{{"extraction_version":"x1","candidates":[
            {{"kind":"fact","content":"User lives in Jaipur","inferred":false,"reason":"stated",
              "evidence":[{{"source":"{src}","span":"I live in Jaipur"}}]}}
        ]}}"#
    )
}

#[tokio::test]
async fn evidence_outside_allowed_sources_rejected() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let seen = source(&mem, "web-page", TrustClass::Untrusted).await;
    let owner = source(&mem, "owner-chat", TrustClass::Owner).await;

    // The extractor was only given the web page but cites the owner conversation.
    let batch = pair_memory::extraction::parse_batch(&batch_json(owner.id)).unwrap();
    let err = mem.propose_batch(batch, &[seen.id]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidInput);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_candidates")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "nothing is stored from a rejected batch");

    let batch = pair_memory::extraction::parse_batch(&batch_json(owner.id)).unwrap();
    let ok = mem
        .propose_batch(batch, &[seen.id, owner.id])
        .await
        .unwrap();
    assert_eq!(ok.len(), 1);
}

#[tokio::test]
async fn webpage_plus_owner_source_cannot_pass_check_accept() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let owner = source(&mem, "owner-chat", TrustClass::Owner).await;
    let web = source(&mem, "web-page", TrustClass::Untrusted).await;

    let proposal = mem
        .propose_with_outcome(
            CandidateDraft::new(MemoryCandidate {
                kind: "preference".into(),
                content: "Deploy target: production by default".into(),
                project: None,
                inferred: false,
                evidence: vec![
                    EvidenceRef {
                        source: owner.id,
                        span: Some("deploy by default".into()),
                    },
                    EvidenceRef {
                        source: web.id,
                        span: Some("production by default".into()),
                    },
                ],
            })
            .with_source_text(owner.id, "deploy by default")
            .with_source_text(web.id, "production by default"),
        )
        .await
        .unwrap();
    assert!(proposal.auto_accepted.is_none());
    let err = mem.accept(proposal.id, "owner").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
}
