#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::testdb::TestDb;
use super::*;
use crate::coding::testkit::{FakeBudget, FakePolicy, FnProvider};
use async_trait::async_trait;
use pair_core::{
    error::Result,
    ids::{TaskId, TraceId},
    money::Micros,
    types::TrustClass,
};
use serde::Deserialize;
use std::sync::Mutex;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../evals/datasets/research"
);

#[derive(Deserialize, Clone)]
struct Page {
    url: String,
    text: String,
    published_at: Option<chrono::NaiveDate>,
    available: bool,
    reason: Option<String>,
}

#[derive(Deserialize, Clone)]
struct Fixture {
    question: String,
    pages: Vec<Page>,
    claims: Vec<RawClaim>,
    statements: Vec<serde_json::Value>,
    injection_marker: Option<String>,
    injected_claim: Option<RawClaim>,
}

fn load(name: &str) -> Fixture {
    serde_json::from_str(&std::fs::read_to_string(format!("{FIXTURE_DIR}/{name}")).unwrap())
        .unwrap()
}

struct FakeFetcher {
    pages: Vec<Page>,
    fetched: Mutex<Vec<String>>,
}

#[async_trait]
impl SourceFetcher for FakeFetcher {
    async fn search(&self, _query: &str) -> Result<Vec<Candidate>> {
        Ok(self
            .pages
            .iter()
            .map(|p| Candidate {
                url: p.url.clone(),
                title: "t".into(),
            })
            .collect())
    }
    async fn fetch(&self, url: &str) -> Result<FetchOutcome> {
        self.fetched.lock().unwrap().push(url.to_string());
        let want = normalize_url(url)?;
        Ok(
            match self
                .pages
                .iter()
                .find(|p| normalize_url(&p.url).ok().as_deref() == Some(want.as_str()))
            {
                Some(p) if p.available => FetchOutcome::Page {
                    text: p.text.clone(),
                    revision: Some("etag-1".into()),
                    published_at: p.published_at,
                },
                Some(p) => FetchOutcome::Unavailable {
                    reason: p.reason.clone().unwrap_or_default(),
                },
                None => FetchOutcome::Unavailable {
                    reason: "404".into(),
                },
            },
        )
    }
}

struct Harness {
    db: TestDb,
    out: ResearchOutput,
    provider: FnProvider,
    fetcher: FakeFetcher,
    policy: FakePolicy,
}

/// Fake model: call 0 extracts the fixture claims (and obeys page injections like a naive
/// model would); call 1 synthesizes the fixture statements.
async fn run_fixture(name: &str, judge: Option<&dyn SupportJudge>) -> Harness {
    let fx = load(name);
    let db = TestDb::create().await;
    let (claims, statements) = (fx.claims.clone(), fx.statements.clone());
    let (marker, injected) = (fx.injection_marker.clone(), fx.injected_claim.clone());
    let provider = FnProvider::new(Box::new(move |i, req| {
        Ok(if i == 0 {
            let mut cs = claims.clone();
            let obeys = marker
                .as_ref()
                .is_some_and(|m| req.messages.iter().any(|x| x.content.contains(m.as_str())));
            if let (true, Some(inj)) = (obeys, injected.clone()) {
                cs.push(inj);
            }
            serde_json::json!({ "claims": cs }).to_string()
        } else {
            serde_json::json!({ "statements": statements }).to_string()
        })
    }));
    let fetcher = FakeFetcher {
        pages: fx.pages.clone(),
        fetched: Mutex::new(Vec::new()),
    };
    let policy = FakePolicy::default();
    let budget = FakeBudget::default();
    let store = EvidenceStore::new(db.pool.clone());
    let deps = ResearchDeps {
        provider: &provider,
        policy: &policy,
        budget: &budget,
        fetcher: &fetcher,
        store: &store,
        judge,
    };
    let run = ResearchRun {
        task: TaskId::new(),
        trace: TraceId::new(),
        model_id: "fake".into(),
        max_call_cost: Micros(10),
    };
    let scope = ResearchScope {
        question: fx.question,
        queries: vec![],
        max_sources: 5,
    };
    let out = run_research(&deps, &run, &scope).await.unwrap();
    Harness {
        db,
        out,
        provider,
        fetcher,
        policy,
    }
}

fn rejected(h: &Harness) -> Vec<&Claim> {
    h.out
        .report
        .claims
        .iter()
        .filter(|c| !c.is_valid())
        .collect()
}

#[tokio::test]
async fn unsupported_citation_rejected() {
    let h = run_fixture("unsupported_citation.json", None).await;
    let rej = rejected(&h);
    assert_eq!(rej.len(), 2);
    let inflated = rej.iter().find(|c| c.raw.value == "40%").unwrap();
    assert!(
        matches!(inflated.rejected, Some(RejectReason::Unsupported(_))),
        "{:?}",
        inflated.rejected
    );
    let invented_span = rej.iter().find(|c| c.raw.value == "doubled").unwrap();
    assert_eq!(invented_span.rejected, Some(RejectReason::SpanNotInSource));
    // only the supported claim reaches the findings
    assert_eq!(h.out.report.statements.len(), 1);
    assert!(
        !h.out.markdown.contains("40%\n")
            && !h
                .out
                .report
                .statements
                .iter()
                .any(|s| s.text.contains("40%"))
    );
    // rejected claims are persisted as rejected, with the reason
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT status, reject_reason FROM research_claims ORDER BY status")
            .fetch_all(&h.db.pool)
            .await
            .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|r| r.0 == "rejected" && r.1.is_some())
            .count(),
        2
    );
}

#[tokio::test]
async fn inaccessible_source_marked() {
    let h = run_fixture("inaccessible_source.json", None).await;
    let dead = h.out.report.sources.iter().find(|s| !s.available).unwrap();
    assert!(dead.unavailable_reason.as_deref().unwrap().contains("403"));
    let claim_b = h
        .out
        .report
        .claims
        .iter()
        .find(|c| c.raw.topic == "launch year b")
        .unwrap();
    assert_eq!(claim_b.rejected, Some(RejectReason::SourceUnavailable));
    assert!(h.out.markdown.contains("UNAVAILABLE"));
    assert!(h
        .out
        .report
        .limitations
        .iter()
        .any(|l| l.contains("inaccessible")));
    let (avail, reason): (bool, Option<String>) = sqlx::query_as(
        "SELECT available, unavailable_reason FROM research_sources WHERE NOT available",
    )
    .fetch_one(&h.db.pool)
    .await
    .unwrap();
    assert!(!avail && reason.unwrap().contains("403"));
}

#[tokio::test]
async fn conflicting_evidence_surfaced() {
    let h = run_fixture("conflicting_evidence.json", None).await;
    let r = &h.out.report;
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].positions.len(), 2);
    // one-sided statement rejected; the statement citing both positions survives
    assert_eq!(r.statements.len(), 1);
    assert_eq!(r.statements[0].claim_ids.len(), 2);
    assert_eq!(r.rejected_statements.len(), 1);
    assert!(r.rejected_statements[0]
        .reason
        .contains("one side of a conflict"));
    // both values appear in the export, nothing is averaged into 2020
    assert!(h.out.markdown.contains("## Conflicting evidence"));
    assert!(h.out.markdown.contains("2019") && h.out.markdown.contains("2021"));
    assert!(!h.out.markdown.contains("2020"));
}

#[tokio::test]
async fn injected_webpage_instruction_ignored() {
    let fx = load("injected_webpage_instruction.json");
    let marker = fx.injection_marker.clone().unwrap();
    let h = run_fixture("injected_webpage_instruction.json", None).await;
    // the plan never changed: only the search result was fetched, never the injected URL
    let fetched = h.fetcher.fetched.lock().unwrap().clone();
    assert_eq!(
        fetched,
        vec!["https://example.com/annual-report".to_string()]
    );
    assert!(h
        .policy
        .seen
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.destination.as_deref() != Some("https://evil.example/exfil")));
    // page text only ever reached the model as untrusted data
    for req in h.provider.seen.lock().unwrap().iter() {
        for m in req.messages.iter().filter(|m| m.content.contains(&marker)) {
            assert_eq!(m.trust, TrustClass::Untrusted);
        }
    }
    // the obeying model's injected claim cites a URL that was never captured -> rejected
    let bad = h
        .out
        .report
        .claims
        .iter()
        .find(|c| c.raw.text == "Acme is bankrupt")
        .unwrap();
    assert_eq!(bad.rejected, Some(RejectReason::InventedUrl));
    assert!(h
        .out
        .report
        .statements
        .iter()
        .all(|s| !s.text.contains("bankrupt")));
    assert_eq!(h.provider.call_count(), 2);
}

#[tokio::test]
async fn invented_url_rejected() {
    let h = run_fixture("invented_url.json", None).await;
    let r = &h.out.report;
    let valid: Vec<_> = r.claims.iter().filter(|c| c.is_valid()).collect();
    // a re-spelled form of a captured URL (case, fragment, trailing slash) is the same source
    assert_eq!(valid.len(), 1);
    assert!(valid[0].raw.url.contains("EXAMPLE.com"));
    let invented: Vec<_> = r
        .claims
        .iter()
        .filter(|c| c.rejected == Some(RejectReason::InventedUrl))
        .collect();
    assert_eq!(invented.len(), 2);
    // inventing a URL must not cause a fetch of it
    assert_eq!(h.fetcher.fetched.lock().unwrap().len(), 1);
    assert_eq!(rejected(&h).len(), 2);
}

type EvidenceRow = (
    String,
    Option<String>,
    Option<chrono::NaiveDate>,
    String,
    String,
    Option<i32>,
);

#[tokio::test]
async fn evidence_tables_store_hash_date_and_exact_span() {
    let h = run_fixture("conflicting_evidence.json", None).await;
    let rows: Vec<EvidenceRow> = sqlx::query_as(
        "SELECT s.content_sha256, s.revision, s.published_at, e.span, e.source_sha256, e.span_start \
         FROM research_claim_evidence e JOIN research_sources s ON s.id = e.source_id ORDER BY e.span",
    )
    .fetch_all(&h.db.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for (hash, revision, date, span, pinned, start) in rows {
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, pinned);
        assert_eq!(revision.as_deref(), Some("etag-1"));
        assert!(date.is_some());
        assert!(span.starts_with("Acme launched the product in"));
        assert_eq!(start, Some(0));
    }
    let md: String = sqlx::query_scalar("SELECT markdown FROM research_reports")
        .fetch_one(&h.db.pool)
        .await
        .unwrap();
    assert_eq!(md, h.out.markdown);
    let status: String = sqlx::query_scalar("SELECT status FROM research_runs")
        .fetch_one(&h.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "completed");
}

struct VetoJudge;
#[async_trait]
impl SupportJudge for VetoJudge {
    async fn judge(&self, _c: &str, _s: &str, _u: &str) -> Result<JudgeVerdict> {
        Ok(JudgeVerdict::DoesNotSupport(
            "paraphrase drifts from source".into(),
        ))
    }
}

#[tokio::test]
async fn judge_hook_can_veto_a_lexically_supported_claim() {
    let h = run_fixture("inaccessible_source.json", Some(&VetoJudge)).await;
    assert!(h
        .out
        .report
        .claims
        .iter()
        .any(|c| matches!(c.rejected, Some(RejectReason::JudgeRejected(_)))));
    assert!(h.out.report.statements.is_empty());
}

#[test]
fn support_check_rules() {
    let src = "Revenue   grew 4% in 2023.\nMargins did not improve.";
    let claim = |text: &str, value: &str, span: &str| RawClaim {
        topic: "t".into(),
        text: text.into(),
        value: value.into(),
        url: "https://x.test/".into(),
        span: span.into(),
    };
    // whitespace/case-insensitive locate, with original offset
    assert_eq!(locate_span(src, "revenue grew 4% in 2023"), Some(0));
    assert!(matches!(
        check_support(
            &claim("Revenue grew 4% in 2023", "4%", "revenue grew 4% in 2023"),
            src
        ),
        Support::Supported { .. }
    ));
    assert!(matches!(
        check_support(
            &claim("Revenue grew 5% in 2023", "5%", "Revenue grew 4% in 2023"),
            src
        ),
        Support::Unsupported(_)
    ));
    assert_eq!(
        check_support(
            &claim("Margins improved", "improved", "Margins improved a lot"),
            src
        ),
        Support::SpanNotInSource
    );
    // negation flip: claim says improved, span says did not improve
    assert!(matches!(
        check_support(
            &claim("Margins did improve", "improve", "Margins did not improve"),
            src
        ),
        Support::Unsupported(_)
    ));
    assert!(matches!(
        check_support(&claim("x", "x", "short"), src),
        Support::Unsupported(_)
    ));
}

#[test]
fn url_normalization_dedupes_spellings_and_rejects_credentials() {
    let a = normalize_url("HTTPS://Example.com:443/a/b/#frag").unwrap();
    assert_eq!(a, "https://example.com/a/b");
    assert_eq!(a, normalize_url("https://example.com/a/b/").unwrap());
    assert!(normalize_url("ftp://example.com/x").is_err());
    assert!(normalize_url("https://user:pw@example.com/").is_err());
    assert!(normalize_url("not a url").is_err());
}
