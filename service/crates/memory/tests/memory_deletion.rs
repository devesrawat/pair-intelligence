//! Spec section 7/11: source deletion invalidates derived memories and indexes, on every read
//! path. Memories and candidates derived solely from deleted sources answer `SourceDeleted`
//! (get) or are omitted (lists, links); mixed ones stay readable minus the deleted spans.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, source, TestDb};
use pair_core::{error::ErrorCode, traits::Memory, types::EvidenceRef, types::TrustClass};
use pair_memory::CandidateDraft;

const SECRET_CONTENT: &str = "The vault passphrase hint is Bluebird";
const SECRET_SPAN: &str = "hint: Bluebird";

#[tokio::test]
async fn deleted_source_memory_not_readable_via_get_memory() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "vault-note", TrustClass::Owner).await;
    let cid = mem
        .propose(candidate("fact", SECRET_CONTENT, None, src.id, SECRET_SPAN))
        .await
        .unwrap();
    let id = mem.accept(cid, "owner").await.unwrap();
    assert_eq!(mem.get_memory(id).await.unwrap().content, SECRET_CONTENT);

    mem.delete_source(src.id, "owner").await.unwrap();

    let err = mem.get_memory(id).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::SourceDeleted);
    assert!(!err.message.contains("Bluebird"));

    // History stays inspectable without content: chain and export carry tombstones.
    let chain = mem.memory_chain(id).await.unwrap();
    assert_eq!(chain.len(), 1);
    assert!(!chain[0].content.contains("Bluebird"));
    assert!(chain[0].evidence.iter().all(|e| e.span.is_none()));
    let export = serde_json::to_string(&mem.export_json().await.unwrap()).unwrap();
    assert!(!export.contains("Bluebird"));
}

#[tokio::test]
async fn deleted_source_candidate_not_readable_via_get_candidate() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "vault-note", TrustClass::Owner).await;
    let cid = mem
        .propose(candidate("fact", SECRET_CONTENT, None, src.id, SECRET_SPAN))
        .await
        .unwrap();
    assert_eq!(
        mem.get_candidate(cid).await.unwrap().content,
        SECRET_CONTENT
    );

    mem.delete_source(src.id, "owner").await.unwrap();

    let err = mem.get_candidate(cid).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::SourceDeleted);
    assert!(!err.message.contains("Bluebird"));
}

#[tokio::test]
async fn deleted_source_candidate_not_listed_in_inbox() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let gone = source(&mem, "vault-note", TrustClass::Owner).await;
    let kept = source(&mem, "other-note", TrustClass::Owner).await;
    mem.propose(candidate(
        "fact",
        SECRET_CONTENT,
        None,
        gone.id,
        SECRET_SPAN,
    ))
    .await
    .unwrap();
    let live = mem
        .propose(candidate(
            "fact",
            "The garage code is unrelated",
            None,
            kept.id,
            "garage",
        ))
        .await
        .unwrap();
    assert_eq!(mem.list_inbox().await.unwrap().len(), 2);

    mem.delete_source(gone.id, "owner").await.unwrap();

    let inbox = mem.list_inbox().await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id, live);
    assert!(!serde_json::to_string(&inbox).unwrap().contains("Bluebird"));
}

#[tokio::test]
async fn memory_with_remaining_live_source_still_readable_without_deleted_span() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let gone = source(&mem, "doc-a", TrustClass::Owner).await;
    let live = source(&mem, "doc-b", TrustClass::Owner).await;
    let mut c = candidate(
        "fact",
        "The staging cluster is in Frankfurt",
        None,
        gone.id,
        "SPAN-ALPHA frankfurt",
    );
    c.evidence.push(EvidenceRef {
        source: live.id,
        span: Some("span-beta eu-central".into()),
    });
    let cand = mem.propose(c).await.unwrap();
    let id = mem.accept(cand, "owner").await.unwrap();

    // A second, still pending candidate with the same two sources.
    let mut p = candidate(
        "fact",
        "Pending note about the cluster",
        None,
        gone.id,
        "SPAN-ALPHA pending",
    );
    p.evidence.push(EvidenceRef {
        source: live.id,
        span: Some("span-beta pending".into()),
    });
    let pending = mem.propose(p).await.unwrap();

    let report = mem.delete_source(gone.id, "owner").await.unwrap();
    assert!(report.invalidated.is_empty());

    let rec = mem.get_memory(id).await.unwrap();
    assert_eq!(rec.content, "The staging cluster is in Frankfurt");
    let dump = serde_json::to_string(&rec.evidence).unwrap();
    assert!(!dump.contains("SPAN-ALPHA"), "deleted span leaked: {dump}");
    assert!(dump.contains("span-beta eu-central"));
    assert!(rec
        .evidence
        .iter()
        .any(|e| e.source_deleted && e.span.is_none()));

    let entry = mem.get_candidate(pending).await.unwrap();
    let dump = serde_json::to_string(&entry.evidence).unwrap();
    assert!(!dump.contains("SPAN-ALPHA"), "deleted span leaked: {dump}");
    assert!(dump.contains("span-beta pending"));
    assert_eq!(mem.list_inbox().await.unwrap().len(), 1);
}

#[tokio::test]
async fn deleted_source_memory_not_shown_in_contradiction_links() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let old_src = source(&mem, "wifi-old", TrustClass::Owner).await;
    let new_src = source(&mem, "wifi-new", TrustClass::Owner).await;
    let mut d = CandidateDraft::new(candidate(
        "fact",
        "The office wifi is named Bluebird",
        None,
        old_src.id,
        "wifi Bluebird",
    ));
    d.topic = Some("office wifi name".into());
    let cid = mem.propose_draft(d).await.unwrap();
    mem.accept(cid, "owner").await.unwrap();
    let mut d = CandidateDraft::new(candidate(
        "fact",
        "The office wifi is named Heron",
        None,
        new_src.id,
        "wifi Heron",
    ));
    d.topic = Some("office wifi name".into());
    let rival = mem.propose_draft(d).await.unwrap();
    assert_eq!(
        mem.get_candidate(rival)
            .await
            .unwrap()
            .contradicts_memories
            .len(),
        1
    );

    mem.delete_source(old_src.id, "owner").await.unwrap();

    let entry = mem.get_candidate(rival).await.unwrap();
    assert!(!serde_json::to_string(&entry).unwrap().contains("Bluebird"));
}

#[tokio::test]
async fn deletion_leaves_no_content_in_chunks_or_audit() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "vault-note", TrustClass::Owner).await;
    let cid = mem
        .propose(candidate("fact", SECRET_CONTENT, None, src.id, SECRET_SPAN))
        .await
        .unwrap();
    mem.accept(cid, "owner").await.unwrap();
    mem.delete_source(src.id, "owner").await.unwrap();

    let chunks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_chunks WHERE text ILIKE '%bluebird%'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(chunks, 0);
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_audit_events WHERE metadata::text ILIKE '%bluebird%'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(audit, 0);
}
