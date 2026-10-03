#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, propose_verified, source, TestDb};
use pair_core::{
    error::ErrorCode,
    traits::Memory,
    types::{MemoryCandidate, TrustClass},
};
use pair_memory::{MemoryStatus, NewSource, Visibility};
use sqlx::Row;

#[tokio::test]
async fn accepted_memory_requires_evidence() {
    let db = TestDb::new().await;
    let mem = db.memory();

    let bare = MemoryCandidate {
        kind: "fact".into(),
        content: "The staging database listens on port 55432".into(),
        project: Some("pair".into()),
        inferred: false,
        evidence: vec![],
    };
    let cid = mem.propose(bare).await.unwrap();
    let err = mem.accept(cid, "owner").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::MemoryNoEvidence);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM memories")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no memory may exist without evidence");

    // The database enforces the same invariant for writers that bypass the service.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO memories (id, kind, status, content, normalized_content, valid_from, observed_at, confidence, accepted_by) \
         VALUES (gen_random_uuid(), 'fact', 'accepted', 'x', 'x', now(), now(), 'observed', 'raw-sql')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    assert!(
        tx.commit().await.is_err(),
        "deferred trigger must reject evidence-less memory"
    );

    // With evidence, accept succeeds and the evidence is inspectable.
    let src = source(&mem, "note-1", TrustClass::Owner).await;
    let cid = propose_verified(
        &mem,
        candidate(
            "fact",
            "The staging database listens on port 55432",
            Some("pair"),
            src.id,
            "staging db: 55432",
        ),
    )
    .await
    .unwrap();
    let mid = mem.accept(cid, "owner").await.unwrap();
    let rec = mem.get_memory(mid).await.unwrap();
    assert_eq!(rec.status, MemoryStatus::Accepted);
    assert_eq!(rec.evidence.len(), 1);
    assert_eq!(rec.evidence[0].span.as_deref(), Some("staging db: 55432"));
}

#[tokio::test]
async fn supersession_preserves_history() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let s1 = source(&mem, "adr-1", TrustClass::Owner).await;
    let s2 = source(&mem, "adr-2", TrustClass::Owner).await;

    let c1 = propose_verified(
        &mem,
        candidate(
            "decision",
            "Decision: use Redis for queues",
            Some("pair"),
            s1.id,
            "use Redis",
        ),
    )
    .await
    .unwrap();
    let old = mem.accept(c1, "owner").await.unwrap();
    let c2 = propose_verified(
        &mem,
        candidate(
            "decision",
            "Decision: use SQS for queues",
            Some("pair"),
            s2.id,
            "use SQS",
        ),
    )
    .await
    .unwrap();
    let new = mem.accept_superseding(c2, "owner", old).await.unwrap();

    let old_rec = mem.get_memory(old).await.unwrap();
    assert_eq!(old_rec.status, MemoryStatus::Superseded);
    assert!(
        old_rec.valid_to.is_some(),
        "validity of the old decision is closed, not deleted"
    );
    assert_eq!(old_rec.content, "Decision: use Redis for queues");
    assert_eq!(old_rec.evidence.len(), 1, "history keeps its evidence");

    let new_rec = mem.get_memory(new).await.unwrap();
    assert_eq!(new_rec.status, MemoryStatus::Accepted);
    assert_eq!(new_rec.supersedes, Some(old));

    let chain = mem.memory_chain(new).await.unwrap();
    let ids: Vec<_> = chain.iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![old, new], "chain is oldest first");

    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_audit_events WHERE subject_kind = 'memory' AND action IN ('memory.accepted', 'memory.superseded')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(audits, 3, "two accepts and one supersession are audited");

    // A memory can only be superseded once.
    let s3 = source(&mem, "adr-3", TrustClass::Owner).await;
    let c3 = propose_verified(
        &mem,
        candidate(
            "decision",
            "Decision: use Kafka for queues",
            Some("pair"),
            s3.id,
            "use Kafka",
        ),
    )
    .await
    .unwrap();
    assert!(mem.accept_superseding(c3, "owner", old).await.is_err());
}

#[tokio::test]
async fn deleted_source_is_not_retrievable() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let only = source(&mem, "doc-a", TrustClass::Owner).await;
    let shared_a = source(&mem, "doc-b", TrustClass::Owner).await;
    let shared_b = source(&mem, "doc-c", TrustClass::Owner).await;

    let c1 = propose_verified(
        &mem,
        candidate(
            "fact",
            "The vault passphrase hint is the dog name",
            None,
            only.id,
            "dog name",
        ),
    )
    .await
    .unwrap();
    let doomed = mem.accept(c1, "owner").await.unwrap();

    let mut two = candidate(
        "fact",
        "The office wifi is named hangar",
        None,
        shared_a.id,
        "wifi hangar",
    );
    two.evidence.push(pair_core::types::EvidenceRef {
        source: shared_b.id,
        span: Some("hangar network".into()),
    });
    let c2 = mem.propose(two).await.unwrap();
    let survivor = mem.accept(c2, "owner").await.unwrap();

    let pending = propose_verified(
        &mem,
        candidate("fact", "Another fact from the doc", None, only.id, "x"),
    )
    .await
    .unwrap();

    let probe = |text: &str| pair_core::types::RetrievalQuery {
        text: text.into(),
        project: None,
        as_of: chrono::Utc::now(),
        limit: 0,
    };
    assert_eq!(
        mem.retrieve_detailed(probe("vault passphrase hint"))
            .await
            .unwrap()
            .len(),
        1
    );

    let report = mem.delete_source(only.id, "owner").await.unwrap();
    assert!(
        mem.retrieve_detailed(probe("vault passphrase hint"))
            .await
            .unwrap()
            .is_empty(),
        "deleted source is not retrievable"
    );
    assert_eq!(report.invalidated, vec![doomed]);

    assert_eq!(
        mem.get_memory(doomed).await.unwrap_err().code,
        ErrorCode::SourceDeleted
    );
    let doomed_rec = mem.memory_chain(doomed).await.unwrap().remove(0);
    assert_eq!(doomed_rec.status, MemoryStatus::Expired);
    assert_eq!(
        doomed_rec.invalidated_reason.as_deref(),
        Some("source_deleted")
    );
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_chunks WHERE memory_id = $1")
        .bind(doomed.0)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        chunks, 0,
        "derived chunks of invalidated memories are removed"
    );
    assert_eq!(
        mem.get_memory(survivor).await.unwrap().status,
        MemoryStatus::Accepted
    );

    // Accepting a candidate whose only source is deleted is refused.
    assert_eq!(
        mem.accept(pending, "owner").await.unwrap_err().code,
        ErrorCode::SourceDeleted
    );
    let late = propose_verified(
        &mem,
        candidate("fact", "Yet another fact", None, only.id, "x"),
    )
    .await;
    assert_eq!(late.unwrap_err().code, ErrorCode::SourceDeleted);

    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_audit_events WHERE action = 'memory.invalidated'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[tokio::test]
async fn source_revisions_track_hash_changes() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let a = mem
        .register_source(NewSource::new(
            "web",
            "https://x.test/p",
            "h1",
            TrustClass::Untrusted,
        ))
        .await
        .unwrap();
    let same = mem
        .register_source(NewSource::new(
            "web",
            "https://x.test/p",
            "h1",
            TrustClass::Untrusted,
        ))
        .await
        .unwrap();
    let b = mem
        .register_source(NewSource::new(
            "web",
            "https://x.test/p",
            "h2",
            TrustClass::Untrusted,
        ))
        .await
        .unwrap();
    assert_eq!(a.id, same.id, "identical hash reuses the revision");
    assert_eq!((a.revision, b.revision), (1, 2));
    assert_ne!(a.id, b.id);

    mem.set_source_visibility(b.id, Visibility::Hidden, "owner")
        .await
        .unwrap();
    let row = sqlx::query("SELECT count(*) AS n FROM sources WHERE external_id = 'https://x.test/p' AND visibility = 'hidden'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<i64, _>("n"),
        2,
        "visibility applies to every revision"
    );
}

#[tokio::test]
async fn export_includes_evidence_and_redacts_deleted_sources() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let keep = source(&mem, "keep", TrustClass::Owner).await;
    let gone = source(&mem, "gone", TrustClass::Owner).await;
    let c1 = propose_verified(
        &mem,
        candidate(
            "preference",
            "I prefer tabs over spaces",
            None,
            keep.id,
            "tabs please",
        ),
    )
    .await
    .unwrap();
    let m1 = mem.accept(c1, "owner").await.unwrap();
    let c2 = propose_verified(
        &mem,
        candidate(
            "fact",
            "Secret project codename is Bluebird",
            None,
            gone.id,
            "codename Bluebird",
        ),
    )
    .await
    .unwrap();
    mem.accept(c2, "owner").await.unwrap();
    mem.delete_source(gone.id, "owner").await.unwrap();

    let json = mem.export_json().await.unwrap();
    let items = json["memories"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let kept = items
        .iter()
        .find(|m| m["id"] == serde_json::json!(m1.to_string()))
        .unwrap();
    assert_eq!(kept["evidence"][0]["span"], "tabs please");

    let md = mem.export_markdown().await.unwrap();
    assert!(md.contains("I prefer tabs over spaces"));
    assert!(md.contains("tabs please"));
    assert!(
        !md.contains("Bluebird"),
        "content and spans derived from deleted sources are redacted"
    );
}
