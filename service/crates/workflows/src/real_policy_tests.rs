//! Workflows against the REAL `PolicyEngine` loaded from `config/policy.yaml` (no fake
//! policy), so a tool name, policy version or egress mismatch fails here instead of in
//! production where every action would be denied.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::coding::{Runner, RunnerSetup};
use crate::research::{
    capture::{capture_sources, Network},
    Candidate, FetchOutcome, ResearchScope, SourceFetcher,
};
use crate::tools::GIT_PUSH;
use async_trait::async_trait;
use pair_core::{
    error::{ErrorCode, Result},
    ids::{ApprovalId, TaskId, TraceId},
    traits::Approvals,
    types::{DataClass, PolicyContext},
};
use pair_jobs::PgApprovals;
use pair_policy::{Gate, PolicyEngine};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const POLICY_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config/policy.yaml");
const JOBS_MIGRATION: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../migrations/030_jobs.sql"
);
const DEFAULT_DB_URL: &str = "postgres://pair:pair@127.0.0.1:55432/pair";
const TIMEOUT: Duration = Duration::from_secs(10);
const ALLOWED_SEARCH_HOST: &str = "api.github.com";
const ALLOWED_PAGE: &str = "https://github.com/rust-lang/rust";
const UNLISTED_PAGE: &str = "https://unlisted.example.org/page";

struct World {
    root: PathBuf,
    repo: PathBuf,
    bare: PathBuf,
    engine: Arc<PolicyEngine>,
}

impl World {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .join(format!("pair_t_real_{}", uuid::Uuid::new_v4().simple()))
            .canonicalize_or_create();
        let (repo, bare, home) = (
            root.join("repo"),
            root.join("remote.git"),
            root.join("home"),
        );
        for d in [&repo, &bare, &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        git(&bare, &["init", "-q", "--bare"]);
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "x\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "i",
            ],
        );
        let engine = PolicyEngine::from_config_file(Path::new(POLICY_PATH), &home).unwrap();
        Self {
            root,
            repo,
            bare,
            engine: Arc::new(engine),
        }
    }

    fn ctx(&self, approvals: Vec<ApprovalId>) -> PolicyContext {
        PolicyContext {
            workspace_root: self.root.display().to_string(),
            approvals,
            policy_version: self.engine.version().to_string(),
        }
    }

    fn runner<'a>(&self, gate: &'a Gate, task: TaskId, approvals: Vec<ApprovalId>) -> Runner<'a> {
        Runner::new(
            gate,
            RunnerSetup {
                task,
                trace: TraceId::new(),
                ctx: self.ctx(approvals),
                home: self.root.join("home"),
                timeout: TIMEOUT,
                passthrough: Vec::new(),
                data_class: DataClass::Personal,
            },
        )
        .with_sandbox(crate::coding::testkit::host_sandbox())
    }

    fn has_remote_branch(&self, branch: &str) -> bool {
        Command::new("git")
            .current_dir(&self.bare)
            .args([
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{branch}"),
            ])
            .status()
            .unwrap()
            .success()
    }

    fn push_argv(&self, refspec: &str) -> Vec<String> {
        ["git", "push", &self.bare.display().to_string(), refspec]
            .map(String::from)
            .to_vec()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

trait CanonicalOrCreate {
    fn canonicalize_or_create(self) -> PathBuf;
}

impl CanonicalOrCreate for PathBuf {
    /// The engine canonicalizes the workspace; compare against the same spelling (macOS /tmp).
    fn canonicalize_or_create(self) -> PathBuf {
        std::fs::create_dir_all(&self).unwrap();
        self.canonicalize().unwrap()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

fn engine_gate(world: &World, approvals: Option<Arc<dyn Approvals>>) -> Gate {
    Gate::new(world.engine.clone(), approvals)
}

#[tokio::test]
async fn real_engine_allows_read_and_workspace_edit_commands_in_coding_runner() {
    let world = World::new();
    let gate = engine_gate(&world, None);
    let runner = world.runner(&gate, TaskId::new(), Vec::new());
    let report = runner
        .run(&argv(&["git", "status"]), &world.repo)
        .await
        .unwrap();
    assert!(report.passed(), "{report:?}");
}

#[tokio::test]
async fn real_engine_denies_unlisted_executable_in_coding_runner() {
    let world = World::new();
    let gate = engine_gate(&world, None);
    let runner = world.runner(&gate, TaskId::new(), Vec::new());
    let err = runner
        .run(&argv(&["sh", "-c", "touch ran.marker"]), &world.repo)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(!world.repo.join("ran.marker").exists());
}

#[tokio::test]
async fn real_engine_requires_approval_for_push() {
    let world = World::new();
    let gate = engine_gate(&world, None);
    let runner = world.runner(&gate, TaskId::new(), Vec::new());
    let err = runner
        .run_tool(
            GIT_PUSH,
            &world.push_argv("main"),
            &world.repo,
            Some("github.com"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert!(!world.has_remote_branch("main"), "push must not have run");
}

#[tokio::test]
async fn real_engine_denies_unregistered_tool() {
    let world = World::new();
    let gate = engine_gate(&world, None);
    let runner = world.runner(&gate, TaskId::new(), Vec::new());
    for tool in ["shell", "git_push", "web_fetch"] {
        let err = runner
            .run_tool(tool, &argv(&["git", "status"]), &world.repo, None)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied, "{tool}");
        assert!(err.message.contains("not registered"), "{}", err.message);
    }
}

#[tokio::test]
async fn real_engine_denies_stale_policy_version() {
    let world = World::new();
    let gate = engine_gate(&world, None);
    let mut ctx = world.ctx(Vec::new());
    ctx.policy_version = "coding-workflow".into();
    let runner = Runner::new(
        &gate,
        RunnerSetup {
            task: TaskId::new(),
            trace: TraceId::new(),
            ctx,
            home: world.root.join("home"),
            timeout: TIMEOUT,
            passthrough: Vec::new(),
            data_class: DataClass::Personal,
        },
    )
    .with_sandbox(crate::coding::testkit::host_sandbox());
    let err = runner
        .run(&argv(&["git", "status"]), &world.repo)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
}

// ---- research -------------------------------------------------------------------------

#[derive(Default)]
struct StubFetcher {
    page: String,
    fetches: AtomicUsize,
}

#[async_trait]
impl SourceFetcher for StubFetcher {
    async fn search(&self, _query: &str) -> Result<Vec<Candidate>> {
        Ok(vec![Candidate {
            url: self.page.clone(),
            title: "t".into(),
        }])
    }
    async fn fetch(&self, _url: &str) -> Result<FetchOutcome> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(FetchOutcome::Page {
            text: "body".into(),
            revision: None,
            published_at: None,
        })
    }
}

async fn research(world: &World, page: &str) -> (Vec<crate::research::Source>, StubFetcher) {
    let gate = engine_gate(world, None);
    let fetcher = StubFetcher {
        page: page.into(),
        ..StubFetcher::default()
    };
    let net = Network {
        gate: &gate,
        fetcher: &fetcher,
        task: TaskId::new(),
        trace: TraceId::new(),
        ctx: world.ctx(Vec::new()),
        search_host: ALLOWED_SEARCH_HOST.into(),
        data_class: DataClass::Public,
    };
    let scope = ResearchScope {
        question: "q".into(),
        queries: vec![],
        max_sources: 3,
        data_class: Some("public".into()),
    };
    let sources = capture_sources(&net, &scope).await.unwrap();
    (sources, fetcher)
}

#[tokio::test]
async fn real_engine_denies_unlisted_egress_host_for_research_fetch() {
    let world = World::new();
    let (sources, fetcher) = research(&world, UNLISTED_PAGE).await;
    assert_eq!(sources.len(), 1);
    assert!(!sources[0].available);
    let reason = sources[0].unavailable_reason.as_deref().unwrap();
    assert!(
        reason.contains("blocked by policy") && reason.contains("allowlist"),
        "{reason}"
    );
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn real_engine_allows_listed_host_for_research_fetch() {
    let world = World::new();
    let (sources, fetcher) = research(&world, ALLOWED_PAGE).await;
    assert!(sources[0].available, "{:?}", sources[0].unavailable_reason);
    assert_eq!(fetcher.fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn real_engine_denies_search_when_service_host_not_listed() {
    let world = World::new();
    let gate = engine_gate(&world, None);
    let fetcher = StubFetcher {
        page: ALLOWED_PAGE.into(),
        ..StubFetcher::default()
    };
    let net = Network {
        gate: &gate,
        fetcher: &fetcher,
        task: TaskId::new(),
        trace: TraceId::new(),
        ctx: world.ctx(Vec::new()),
        search_host: "search.unlisted.example.org".into(),
        data_class: DataClass::Public,
    };
    let scope = ResearchScope {
        question: "q".into(),
        queries: vec![],
        max_sources: 3,
        data_class: Some("public".into()),
    };
    let err = capture_sources(&net, &scope).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
}

// ---- approvals (real PgApprovals) --------------------------------------------------------

struct ApprovalDb {
    pool: PgPool,
    name: String,
    admin_url: String,
}

impl ApprovalDb {
    async fn create() -> Self {
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());
        let base = url
            .split('?')
            .next()
            .unwrap()
            .rsplit_once('/')
            .unwrap()
            .0
            .to_string();
        let admin_url = format!("{base}/postgres");
        let name = format!("pair_t_wfappr_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&format!("{base}/{name}"))
            .await
            .unwrap();
        let sql = std::fs::read_to_string(JOBS_MIGRATION).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        Self {
            pool,
            name,
            admin_url,
        }
    }

    async fn drop_db(self) {
        self.pool.close().await;
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.admin_url)
            .await
            .unwrap();
        sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ))
        .execute(&admin)
        .await
        .unwrap();
    }
}

/// The hash an approver must bind to, taken from the Gate's own refusal message.
async fn required_hash(world: &World, task: TaskId, refspec: &str) -> String {
    let gate = engine_gate(world, None);
    let runner = world.runner(&gate, task, Vec::new());
    let err = runner
        .run_tool(
            GIT_PUSH,
            &world.push_argv(refspec),
            &world.repo,
            Some("github.com"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    err.message.rsplit(' ').next().unwrap().to_string()
}

#[tokio::test]
async fn approved_push_executes_once_and_modified_command_or_reuse_is_denied() {
    let db = ApprovalDb::create().await;
    let world = World::new();
    let approvals = Arc::new(PgApprovals::new(db.pool.clone()));
    let gate = engine_gate(&world, Some(approvals.clone()));
    let task = TaskId::new();
    let push = |refspec: &'static str, ids: Vec<ApprovalId>| {
        let runner = world.runner(&gate, task, ids);
        let argv = world.push_argv(refspec);
        let repo = world.repo.clone();
        async move {
            runner
                .run_tool(GIT_PUSH, &argv, &repo, Some("github.com"))
                .await
        }
    };

    let hash = required_hash(&world, task, "main").await;
    let id = approvals.approve_default(&hash, "owner").await.unwrap();

    // a modified command does not match the approved hash, and must not burn the approval
    let err = push("main:evil", vec![id]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert!(!world.has_remote_branch("evil"));
    assert!(!world.has_remote_branch("main"));

    // the exact command executes exactly once
    let report = push("main", vec![id]).await.unwrap();
    assert!(report.passed(), "{report:?}");
    assert!(world.has_remote_branch("main"));

    // reuse of the same approval is denied and does not run
    git(&world.bare, &["update-ref", "-d", "refs/heads/main"]);
    let err = push("main", vec![id]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert!(
        !world.has_remote_branch("main"),
        "second use must not execute"
    );

    db.drop_db().await;
}
