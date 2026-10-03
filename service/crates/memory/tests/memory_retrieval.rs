#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use chrono::{Duration, Utc};
use common::{candidate, days_ago, propose_verified, source, TestDb};
use pair_core::{
    ids::MemoryId,
    traits::Memory,
    types::{EvidenceRef, EvidenceStatus, RetrievalQuery, TrustClass},
};
use pair_memory::{store::AcceptMode, CandidateDraft, PgMemory, Visibility};

fn query(text: &str, project: Option<&str>) -> RetrievalQuery {
    RetrievalQuery {
        text: text.into(),
        project: project.map(str::to_string),
        as_of: Utc::now(),
        limit: 0,
    }
}

async fn accept_fact(
    mem: &PgMemory,
    kind: &str,
    content: &str,
    project: Option<&str>,
    ext: &str,
    span: &str,
) -> (MemoryId, pair_core::ids::SourceId) {
    let src = source(mem, ext, TrustClass::Owner).await;
    let cid = propose_verified(&mem, candidate(kind, content, project, src.id, span))
        .await
        .unwrap();
    (mem.accept(cid, "owner").await.unwrap(), src.id)
}

#[tokio::test]
async fn project_filter() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let (a, _) = accept_fact(
        &mem,
        "fact",
        "Alpha deploys run on Fridays",
        Some("alpha"),
        "a",
        "fridays",
    )
    .await;
    let (_b, _) = accept_fact(
        &mem,
        "fact",
        "Beta deploys run on Mondays",
        Some("beta"),
        "b",
        "mondays",
    )
    .await;
    let (g, _) = accept_fact(
        &mem,
        "fact",
        "Deploys always need a changelog entry",
        None,
        "g",
        "changelog",
    )
    .await;

    let hits = mem
        .retrieve_detailed(query("when do deploys run", Some("alpha")))
        .await
        .unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.item.memory).collect();
    assert!(ids.contains(&a));
    assert!(ids.contains(&g), "global memories apply to every project");
    assert_eq!(ids.len(), 2, "other projects never leak");

    let none = mem
        .retrieve_detailed(query("when do deploys run", None))
        .await
        .unwrap();
    assert_eq!(
        none.iter().map(|h| h.item.memory).collect::<Vec<_>>(),
        vec![g],
        "no project means global only"
    );
}

#[tokio::test]
async fn expired_memory_excluded() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "s", TrustClass::Owner).await;

    let mut d = CandidateDraft::new(candidate(
        "fact",
        "The VPN gateway address is vpn-old.example",
        None,
        src.id,
        "vpn-old",
    ));
    d.valid_from = Some(days_ago(60));
    d.valid_to = Some(days_ago(10));
    let old = mem
        .accept(mem.propose_draft(d).await.unwrap(), "owner")
        .await
        .unwrap();

    let mut f = CandidateDraft::new(candidate(
        "fact",
        "The wiki gateway address starts next month",
        None,
        src.id,
        "future",
    ));
    f.valid_from = Some(Utc::now() + Duration::days(30));
    mem.accept(mem.propose_draft(f).await.unwrap(), "owner")
        .await
        .unwrap();

    let now_hits = mem
        .retrieve_detailed(query("vpn gateway address", None))
        .await
        .unwrap();
    assert!(
        now_hits.is_empty(),
        "expired validity is excluded as of now"
    );
    assert!(
        mem.retrieve_detailed(query("wiki gateway address", None))
            .await
            .unwrap()
            .is_empty(),
        "not yet valid is excluded"
    );

    let mut past = query("vpn gateway address", None);
    past.as_of = days_ago(30);
    let past_hits = mem.retrieve_detailed(past).await.unwrap();
    assert_eq!(
        past_hits.iter().map(|h| h.item.memory).collect::<Vec<_>>(),
        vec![old],
        "as_of selects the then-valid memory"
    );

    // Explicit lifecycle expiry behaves the same.
    let (live, _) = accept_fact(
        &mem,
        "fact",
        "The backup window is 02:00 UTC",
        None,
        "b",
        "backup",
    )
    .await;
    assert_eq!(
        mem.retrieve_detailed(query("backup window", None))
            .await
            .unwrap()
            .len(),
        1
    );
    mem.expire_memory(live, "owner", "window changed")
        .await
        .unwrap();
    assert!(mem
        .retrieve_detailed(query("backup window", None))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn conflicting_decisions_flagged() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let s1 = source(&mem, "adr-1", TrustClass::Owner).await;
    let s2 = source(&mem, "adr-2", TrustClass::Owner).await;
    let c1 = propose_verified(
        &mem,
        candidate(
            "decision",
            "Queue technology: Redis streams",
            Some("pair"),
            s1.id,
            "use Redis streams",
        ),
    )
    .await
    .unwrap();
    let old = mem.accept(c1, "owner").await.unwrap();
    let c2 = propose_verified(
        &mem,
        candidate(
            "decision",
            "Queue technology: SQS with DLQ",
            Some("pair"),
            s2.id,
            "use SQS",
        ),
    )
    .await
    .unwrap();
    let new = mem.accept_superseding(c2, "owner", old).await.unwrap();

    let hits = mem
        .retrieve_detailed(query(
            "which queue technology did we decide on",
            Some("pair"),
        ))
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        2,
        "both the current and the superseded decision are returned"
    );
    assert_eq!(hits[0].item.memory, new);
    assert!(hits[0].current);
    assert_eq!(
        hits[0].supersedes,
        Some(old),
        "history is linked from the current decision"
    );
    assert_eq!(hits[1].item.memory, old);
    assert!(!hits[1].current);
    assert_eq!(hits[1].item.superseded_by, Some(new));
    assert!(hits.iter().all(|h| !h.item.evidence.is_empty()));

    // Querying only with words of the old decision still surfaces the replacement first.
    let only_old = mem
        .retrieve_detailed(query("Redis streams", Some("pair")))
        .await
        .unwrap();
    assert_eq!(only_old[0].item.memory, new);
    assert!(only_old[0].current);

    // As of before the replacement, the old decision is the current one.
    let mut past = query("which queue technology did we decide on", Some("pair"));
    past.as_of = old_valid_from(&mem, old).await + Duration::milliseconds(1);
    let past_hits = mem.retrieve_detailed(past).await.unwrap();
    assert_eq!(past_hits.len(), 1);
    assert_eq!(past_hits[0].item.memory, old);
    assert!(past_hits[0].current);

    // Competing decisions deliberately kept side by side are flagged as conflicting.
    let s3 = source(&mem, "adr-3", TrustClass::Owner).await;
    let s4 = source(&mem, "adr-4", TrustClass::Owner).await;
    let j = propose_verified(
        &mem,
        candidate(
            "decision",
            "Auth mode: JWT bearer tokens",
            Some("pair"),
            s3.id,
            "jwt",
        ),
    )
    .await
    .unwrap();
    let jwt = mem.accept(j, "owner").await.unwrap();
    let s = propose_verified(
        &mem,
        candidate(
            "decision",
            "Auth mode: server sessions",
            Some("pair"),
            s4.id,
            "sessions",
        ),
    )
    .await
    .unwrap();
    let sess = mem
        .accept_with(
            s,
            "owner",
            AcceptMode {
                keep_both: true,
                ..AcceptMode::default()
            },
        )
        .await
        .unwrap();
    let auth = mem
        .retrieve_detailed(query("auth mode", Some("pair")))
        .await
        .unwrap();
    assert_eq!(auth.len(), 2);
    let flagged = auth.iter().find(|h| h.item.memory == jwt).unwrap();
    assert_eq!(flagged.item.conflicts_with, vec![sess]);
    assert!(auth.iter().all(|h| h.current));
}

async fn old_valid_from(mem: &PgMemory, id: MemoryId) -> chrono::DateTime<Utc> {
    mem.get_memory(id).await.unwrap().valid_from
}

#[tokio::test]
async fn source_permission_change_hides_memory() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let (m, src) = accept_fact(
        &mem,
        "fact",
        "The audit report lives in the finance share",
        None,
        "fin",
        "finance share",
    )
    .await;
    assert_eq!(
        mem.retrieve_detailed(query("audit report location", None))
            .await
            .unwrap()
            .len(),
        1
    );

    mem.set_source_visibility(src, Visibility::Hidden, "owner")
        .await
        .unwrap();
    assert!(mem
        .retrieve_detailed(query("audit report location", None))
        .await
        .unwrap()
        .is_empty());
    assert!(mem
        .retrieve(query("audit report location", None))
        .await
        .unwrap()
        .is_empty());

    mem.set_source_visibility(src, Visibility::Visible, "owner")
        .await
        .unwrap();
    assert_eq!(
        mem.retrieve_detailed(query("audit report location", None))
            .await
            .unwrap()[0]
            .item
            .memory,
        m
    );

    // With two sources, only the visible evidence is returned.
    let second = source(&mem, "fin-2", TrustClass::Owner).await;
    let mut c = candidate(
        "fact",
        "The payroll export runs nightly",
        None,
        src,
        "payroll nightly",
    );
    c.evidence.push(EvidenceRef {
        source: second.id,
        span: Some("nightly payroll job".into()),
    });
    let cid = mem.propose(c).await.unwrap();
    mem.accept(cid, "owner").await.unwrap();
    mem.set_source_visibility(src, Visibility::Hidden, "owner")
        .await
        .unwrap();
    let hits = mem
        .retrieve_detailed(query("payroll export", None))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item.evidence.len(), 1);
    assert_eq!(hits[0].item.evidence[0].source, second.id);
}

#[tokio::test]
async fn deleted_source_memory_is_not_retrieved() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let (_, src) = accept_fact(
        &mem,
        "fact",
        "The safe combination hint is the street name",
        None,
        "safe",
        "street",
    )
    .await;
    assert_eq!(
        mem.retrieve_detailed(query("safe combination hint", None))
            .await
            .unwrap()
            .len(),
        1
    );
    mem.delete_source(src, "owner").await.unwrap();
    assert!(mem
        .retrieve_detailed(query("safe combination hint", None))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn results_are_capped_and_carry_spans() {
    let db = TestDb::new().await;
    let mem = db.memory();
    for i in 0..12 {
        accept_fact(
            &mem,
            "fact",
            &format!("Release checklist item {i} covers smoke testing"),
            None,
            &format!("r{i}"),
            &format!("checklist span {i}"),
        )
        .await;
    }
    let hits = mem
        .retrieve_detailed(query("release checklist smoke testing", None))
        .await
        .unwrap();
    assert_eq!(hits.len(), 8, "default cap is 8");
    assert!(hits
        .iter()
        .all(|h| h.item.evidence.iter().all(|e| e.span.is_some()) && !h.item.evidence.is_empty()));
    let mut q = query("release checklist smoke testing", None);
    q.limit = 3;
    assert_eq!(mem.retrieve_detailed(q).await.unwrap().len(), 3);
    assert!(mem
        .retrieve_detailed(query("quantum chromodynamics", None))
        .await
        .unwrap()
        .is_empty());
    assert!(mem
        .retrieve_detailed(query("the of and", None))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn inferred_label_survives_retrieval() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "s", TrustClass::Owner).await;
    let mut c = candidate(
        "fact",
        "The user is likely preparing for a conference talk",
        None,
        src.id,
        "slides draft",
    );
    c.inferred = true;
    mem.accept(mem.propose(c).await.unwrap(), "owner")
        .await
        .unwrap();
    let hits = mem
        .retrieve_detailed(query("conference talk", None))
        .await
        .unwrap();
    assert!(hits[0].item.inferred);
}

#[tokio::test]
async fn retrieve_content_has_no_label_prefixes_and_fields_are_set() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let (old, _) = accept_fact(
        &mem,
        "decision",
        "Queue technology: Redis streams",
        Some("pair"),
        "q1",
        "use Redis streams",
    )
    .await;
    let s2 = source(&mem, "q2", TrustClass::Owner).await;
    let c2 = propose_verified(
        &mem,
        candidate(
            "decision",
            "Queue technology: SQS with DLQ",
            Some("pair"),
            s2.id,
            "use SQS",
        ),
    )
    .await
    .unwrap();
    let new = mem.accept_superseding(c2, "owner", old).await.unwrap();

    let items = mem
        .retrieve(query(
            "which queue technology did we decide on",
            Some("pair"),
        ))
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].memory, new);
    assert_eq!(items[0].status, EvidenceStatus::Current);
    assert_eq!(items[0].superseded_by, None);
    assert_eq!(items[1].memory, old);
    assert_eq!(items[1].status, EvidenceStatus::Superseded);
    assert_eq!(items[1].superseded_by, Some(new));
    assert_eq!(items[1].content, "Queue technology: Redis streams");
    assert_eq!(items[0].content, "Queue technology: SQS with DLQ");

    let (jwt, _) = accept_fact(
        &mem,
        "decision",
        "Auth mode: JWT bearer tokens",
        Some("pair"),
        "a1",
        "jwt",
    )
    .await;
    let s4 = source(&mem, "a2", TrustClass::Owner).await;
    let sc = propose_verified(
        &mem,
        candidate(
            "decision",
            "Auth mode: server sessions",
            Some("pair"),
            s4.id,
            "sessions",
        ),
    )
    .await
    .unwrap();
    let sess = mem
        .accept_with(
            sc,
            "owner",
            AcceptMode {
                keep_both: true,
                ..AcceptMode::default()
            },
        )
        .await
        .unwrap();
    let auth = mem
        .retrieve(query("auth mode", Some("pair")))
        .await
        .unwrap();
    assert_eq!(auth.len(), 2);
    let jwt_item = auth.iter().find(|i| i.memory == jwt).unwrap();
    assert_eq!(jwt_item.status, EvidenceStatus::Conflicting);
    assert_eq!(jwt_item.conflicts_with, vec![sess]);
    assert_eq!(jwt_item.content, "Auth mode: JWT bearer tokens");

    let src = source(&mem, "inf", TrustClass::Owner).await;
    let mut c = candidate(
        "fact",
        "The user is likely preparing for a conference talk",
        None,
        src.id,
        "slides draft",
    );
    c.inferred = true;
    mem.accept(mem.propose(c).await.unwrap(), "owner")
        .await
        .unwrap();
    let inferred = mem.retrieve(query("conference talk", None)).await.unwrap();
    assert!(inferred[0].inferred);
    assert_eq!(inferred[0].status, EvidenceStatus::Current);
    assert_eq!(
        inferred[0].content,
        "The user is likely preparing for a conference talk"
    );
    assert!(items
        .iter()
        .chain(&auth)
        .all(|i| !i.content.starts_with('[') && !i.inferred));
}

#[tokio::test]
async fn coverage_counts_across_chunks() {
    let db = TestDb::new().await;
    let mem = db.memory();
    // Two query terms live in the memory text, two others only in the evidence span.
    let (id, _) = accept_fact(
        &mem,
        "fact",
        "Kubernetes deployment",
        None,
        "k8s",
        "frankfurt region",
    )
    .await;
    // Five terms need three matches; no single chunk has more than two.
    let hits = mem
        .retrieve_detailed(query(
            "kubernetes deployment frankfurt region unrelated",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item.memory, id);

    // Terms matched in no chunk still do not count: 2 of 5 stays below the gate.
    let weak = mem
        .retrieve_detailed(query("kubernetes frankfurt alpha beta gamma", None))
        .await
        .unwrap();
    assert!(weak.is_empty());
}

#[tokio::test]
async fn conflict_partner_beyond_limit_still_labels_conflicting() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let s1 = source(&mem, "adr-3", TrustClass::Owner).await;
    let s2 = source(&mem, "adr-4", TrustClass::Owner).await;
    let first = propose_verified(
        &mem,
        candidate(
            "decision",
            "Auth mode: JWT bearer",
            Some("pair"),
            s1.id,
            "jwt",
        ),
    )
    .await
    .unwrap();
    let jwt = mem.accept(first, "owner").await.unwrap();
    let second = propose_verified(
        &mem,
        candidate(
            "decision",
            "Auth mode: server sessions",
            Some("pair"),
            s2.id,
            "sessions",
        ),
    )
    .await
    .unwrap();
    let sess = mem
        .accept_with(
            second,
            "owner",
            AcceptMode {
                keep_both: true,
                ..AcceptMode::default()
            },
        )
        .await
        .unwrap();

    // Only one slot: the partner is cut by the limit but is still a valid, visible conflict.
    let mut q = query("auth mode", Some("pair"));
    q.limit = 1;
    let hits = mem.retrieve_detailed(q).await.unwrap();
    assert_eq!(hits.len(), 1);
    let other = if hits[0].item.memory == jwt {
        sess
    } else {
        jwt
    };
    assert_eq!(hits[0].item.status, EvidenceStatus::Conflicting);
    assert_eq!(hits[0].item.conflicts_with, vec![other]);
}
