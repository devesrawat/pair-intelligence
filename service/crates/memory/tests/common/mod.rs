//! Shared test harness: one throwaway database per test, migrated and dropped afterwards.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
use chrono::{DateTime, Duration, Utc};
use pair_core::{
    ids::{CandidateId, SourceId},
    types::{EvidenceRef, MemoryCandidate, TrustClass},
};
use pair_memory::{CandidateDraft, NewSource, PgMemory, SourceRecord};
use sqlx::{postgres::PgPoolOptions, Connection, PgConnection, PgPool};
use std::path::PathBuf;
use uuid::Uuid;

const DEFAULT_DATABASE_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";

pub struct TestDb {
    pub pool: PgPool,
    admin_url: String,
    name: String,
}

fn migrations_dir() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Repo-root `migrations/` is canonical (spec section 15); accept `service/migrations` too,
    // but only when it actually holds the memory migrations.
    [
        root.join("../../../migrations"),
        root.join("../../migrations"),
    ]
    .into_iter()
    .find(|dir| dir.join("020_sources.sql").is_file())
    .expect(
        "memory migrations (020_sources.sql) not found in ../../../migrations or ../../migrations",
    )
}

impl TestDb {
    pub async fn new() -> Self {
        let admin_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());
        let name = format!("pair_t_memory_{}", Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(&admin_url).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let (base, _) = admin_url.rsplit_once('/').unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&format!("{base}/{name}"))
            .await
            .unwrap();
        let migrator = sqlx::migrate::Migrator::new(migrations_dir())
            .await
            .unwrap();
        migrator.run(&pool).await.unwrap();
        Self {
            pool,
            admin_url,
            name,
        }
    }

    pub fn memory(&self) -> PgMemory {
        PgMemory::new(self.pool.clone())
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let url = self.admin_url.clone();
        let name = self.name.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut admin = PgConnection::connect(&url).await.unwrap();
                sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                    .execute(&mut admin)
                    .await
                    .unwrap();
            });
        });
        let _ = handle.join();
    }
}

pub async fn source(mem: &PgMemory, ext: &str, trust: TrustClass) -> SourceRecord {
    mem.register_source(NewSource::new("note", ext, format!("hash-{ext}"), trust))
        .await
        .unwrap()
}

pub fn candidate(
    kind: &str,
    content: &str,
    project: Option<&str>,
    src: SourceId,
    span: &str,
) -> MemoryCandidate {
    MemoryCandidate {
        kind: kind.to_string(),
        content: content.to_string(),
        project: project.map(str::to_string),
        inferred: false,
        evidence: vec![EvidenceRef {
            source: src,
            span: Some(span.to_string()),
        }],
    }
}

/// A draft whose evidence spans are quoted from the (simulated) source text, so they verify.
pub fn verified(c: MemoryCandidate) -> CandidateDraft {
    let texts: Vec<(SourceId, String)> = c
        .evidence
        .iter()
        .filter_map(|e| e.span.clone().map(|s| (e.source, s)))
        .collect();
    let mut draft = CandidateDraft::new(c);
    for (src, text) in texts {
        draft = draft.with_source_text(src, text);
    }
    draft
}

pub async fn propose_verified(
    mem: &PgMemory,
    c: MemoryCandidate,
) -> pair_core::error::Result<CandidateId> {
    Ok(mem.propose_with_outcome(verified(c)).await?.id)
}

pub fn days_ago(n: i64) -> DateTime<Utc> {
    Utc::now() - Duration::days(n)
}
