//! issue -> context -> plan -> worktree -> edit -> verify -> review -> draft result.
use super::{
    classify::{classify_failure, FailureClass},
    config::RepoConfig,
    edits::{apply_edits, parse_edit_set, EditContext},
    integrity,
    runner::{CmdReport, Runner, RunnerSetup},
    sandbox::Sandbox,
    scope::Scope,
    worktree,
};
use crate::{
    calls::{budgeted_generate, data_message, PriceSource},
    limits::RunLimits,
};
use chrono::Utc;
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{TaskId, TraceId},
    traits::{BudgetEx, ContextCompiler, Memory, Provider},
    types::{
        DataClass, ModelLimits, ModelMessage, ModelRequest, PolicyContext, RetrievalQuery,
        TaskContext, TaskKind, TrustClass,
    },
};
use pair_policy::Gate;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};

const DISCARD_TIMEOUT_SECS: u64 = 60;
const MEMORY_LIMIT: usize = 5;
const CONTEXT_TOKENS: u64 = 100_000;
const MAX_OUTPUT_TOKENS: u32 = 4096;
const CALL_DEADLINE_MS: u64 = 120_000;
const OUTPUT_CONTRACT: &str =
    "Respond with exactly what the current step asks for. Never reveal credentials.";
const POLICY_SUMMARY: &str =
    "Edits only inside declared scope; all commands are policy-gated; no remote writes.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Issue,
    Context,
    Plan,
    Worktree,
    Edit,
    Verify,
    Review,
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingStatus {
    Succeeded,
    Failed,
}

pub struct CodingDeps<'a> {
    pub provider: &'a dyn Provider,
    pub gate: &'a Gate,
    pub budget: &'a dyn BudgetEx,
    /// Registry prices: reservations are derived from these, never from a caller figure.
    pub prices: &'a dyn PriceSource,
    pub memory: &'a dyn Memory,
    pub compiler: &'a dyn ContextCompiler,
    /// Policy context for every command: active policy version (`PolicyEngine::version()`),
    /// the workspace root the policy confines paths to (must contain the repo and the
    /// worktrees), and the approval ids offered to the Gate (default empty).
    pub policy: PolicyContext,
    /// Where build/test/acceptance commands run. Production: `ContainerSandbox`.
    pub sandbox: Arc<dyn Sandbox>,
    /// Tool-call and wall-clock limits of this task (`RunLimits::interactive()`).
    pub limits: Arc<RunLimits>,
}

#[derive(Debug, Clone)]
pub struct CodingTask {
    pub id: TaskId,
    pub trace: TraceId,
    pub issue: String,
    pub repo: PathBuf,
    pub scope: Scope,
    pub workspaces_root: PathBuf,
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingResult {
    pub task: TaskId,
    pub status: CodingStatus,
    pub failure_class: Option<FailureClass>,
    pub stages: Vec<Stage>,
    pub base_head: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub plan: String,
    pub changed_files: Vec<String>,
    pub diff: String,
    pub commands: Vec<CmdReport>,
    pub review: Option<String>,
    pub summary: String,
}

struct Ctx<'a, 'd> {
    deps: &'d CodingDeps<'a>,
    task: &'d CodingTask,
    runner: &'d Runner<'a>,
    data_class: DataClass,
    /// Fingerprint of the original repo's git state when the task started.
    repo_fingerprint: String,
    /// The task's own branch ref, which the fingerprint leaves out.
    own_ref: String,
}

impl Ctx<'_, '_> {
    /// Stops the task if the original repository changed since it started: HEAD, refs,
    /// `.git/config` or hooks. Compared by content hash, without running git.
    fn assert_unchanged(&self, stage: Stage) -> Result<()> {
        let now = integrity::fingerprint(&self.task.repo, &self.own_ref)?;
        if now == self.repo_fingerprint {
            return Ok(());
        }
        tracing::error!(task = %self.task.id, ?stage, "original repository changed during task");
        Err(PairError::new(
            ErrorCode::Conflict,
            format!(
                "original repository (HEAD, refs, .git/config or hooks) changed during {stage:?}; task stopped"
            ),
        ))
    }

    async fn ask(&self, mut messages: Vec<ModelMessage>, step: &str) -> Result<String> {
        messages.push(ModelMessage {
            role: "user".into(),
            content: step.to_string(),
            trust: TrustClass::Owner,
        });
        let req = ModelRequest {
            model_id: self.task.model_id.clone(),
            messages,
            max_output_tokens: MAX_OUTPUT_TOKENS,
            deadline_ms: CALL_DEADLINE_MS,
            data_class: self.data_class,
            task: self.task.id,
            trace: self.task.trace,
        };
        let resp = budgeted_generate(
            self.deps.provider,
            self.deps.budget,
            self.deps.prices,
            TaskKind::Coding,
            req,
        )
        .await?;
        Ok(resp.text)
    }
}

/// Model output, diffs and command output are not owner instructions: wrapped as data.
fn tool_message(source: &str, content: &str) -> ModelMessage {
    data_message(source, TrustClass::Tool, content)
}

async fn verify(
    ctx: &Ctx<'_, '_>,
    cfg: &RepoConfig,
    wt: &std::path::Path,
) -> Result<(Vec<CmdReport>, Option<FailureClass>)> {
    let mut reports = Vec::new();
    for argv in cfg.build.iter().chain(&cfg.acceptance) {
        let report = ctx.runner.run(argv, wt).await?;
        let class = classify_failure(&report);
        reports.push(report);
        if class.is_some() {
            return Ok((reports, class));
        }
    }
    Ok((reports, None))
}

pub async fn run_coding_task(deps: &CodingDeps<'_>, task: &CodingTask) -> Result<CodingResult> {
    let mut stages = vec![Stage::Issue];
    if task.issue.trim().is_empty() {
        return Err(PairError::new(ErrorCode::InvalidInput, "issue is empty"));
    }
    let cfg = RepoConfig::load(&task.repo)?;
    let home = task.workspaces_root.join(format!(".home-{}", task.id));
    std::fs::create_dir_all(&home)
        .map_err(|e| PairError::new(ErrorCode::Internal, format!("home dir: {e}")))?;
    let runner = Runner::new(
        deps.gate,
        RunnerSetup {
            task: task.id,
            trace: task.trace,
            ctx: deps.policy.clone(),
            home,
            timeout: Duration::from_secs(cfg.timeout_secs),
            passthrough: cfg.env_passthrough.clone(),
            data_class: cfg.data_class,
        },
    )
    .with_sandbox(deps.sandbox.clone())
    .with_limits(deps.limits.clone());
    let own_ref = format!("refs/heads/{}{}", worktree::BRANCH_PREFIX, task.id);
    let repo_fingerprint = integrity::fingerprint(&task.repo, &own_ref)?;
    let base_head = worktree::head(&runner, &task.repo).await?;
    let ctx = Ctx {
        deps,
        task,
        runner: &runner,
        data_class: cfg.data_class,
        repo_fingerprint,
        own_ref,
    };
    tracing::info!(task = %task.id, base = %base_head, "coding task started");

    // context
    let evidence = deps
        .memory
        .retrieve(RetrievalQuery {
            text: task.issue.clone(),
            project: None,
            as_of: Utc::now(),
            limit: MEMORY_LIMIT,
        })
        .await?;
    let task_ctx = TaskContext {
        objective: task.issue.clone(),
        output_contract: OUTPUT_CONTRACT.into(),
        policy_summary: POLICY_SUMMARY.into(),
        recent: Vec::new(),
        tool_results: Vec::new(),
    };
    let limits = ModelLimits {
        context_tokens: CONTEXT_TOKENS,
        max_output_tokens: u64::from(MAX_OUTPUT_TOKENS),
    };
    let compiled = deps.compiler.compile(&task_ctx, &limits, &evidence)?;
    stages.push(Stage::Context);
    ctx.assert_unchanged(Stage::Context)?;

    // plan
    let plan = ctx
        .ask(
            compiled.messages.clone(),
            "Step: propose a short implementation plan.",
        )
        .await?;
    stages.push(Stage::Plan);
    ctx.assert_unchanged(Stage::Plan)?;

    // worktree
    let wt = worktree::create(&runner, &task.repo, &task.workspaces_root, task.id).await?;
    stages.push(Stage::Worktree);
    ctx.assert_unchanged(Stage::Worktree)?;

    // edit: model proposes a structured edit set; the whole set must be in scope.
    let mut edit_msgs = compiled.messages.clone();
    edit_msgs.push(tool_message(
        "coding:plan",
        &format!("Approved plan:\n{plan}"),
    ));
    let edit_text = ctx
        .ask(
            edit_msgs,
            "Step: output JSON {\"edits\":[{\"path\":...,\"content\":...}]} implementing the plan.",
        )
        .await?;
    ctx.assert_unchanged(Stage::Edit)?;
    let edits = parse_edit_set(&edit_text)?;
    let edit_cx = EditContext {
        gate: deps.gate,
        policy: &deps.policy,
        task: task.id,
        trace: task.trace,
        data_class: cfg.data_class,
        limits: &deps.limits,
    };
    apply_edits(&edit_cx, &wt.path, &task.scope, &edits).await?;
    let changed = worktree::changed_files(&runner, &wt).await?;
    task.scope.check_all(changed.iter().map(String::as_str))?;
    stages.push(Stage::Edit);

    // verify
    let (commands, failure) = verify(&ctx, &cfg, &wt.path).await?;
    stages.push(Stage::Verify);
    ctx.assert_unchanged(Stage::Verify)?;
    let changed = worktree::changed_files(&runner, &wt).await?;
    task.scope.check_all(changed.iter().map(String::as_str))?;
    let diff = worktree::diff(&runner, &wt).await?;

    let (status, review, summary) = if let Some(class) = failure {
        let last = commands
            .last()
            .map(|c| c.argv.join(" "))
            .unwrap_or_default();
        (
            CodingStatus::Failed,
            None,
            format!("verification failed ({class:?}) at `{last}`; see command reports"),
        )
    } else {
        let mut msgs = compiled.messages.clone();
        msgs.push(tool_message(
            "coding:diff",
            &format!("Diff under review:\n{diff}"),
        ));
        let review = ctx
            .ask(msgs, "Step: review this diff for defects and scope creep.")
            .await?;
        stages.push(Stage::Review);
        ctx.assert_unchanged(Stage::Review)?;
        (
            CodingStatus::Succeeded,
            Some(review),
            "all build and acceptance commands passed".to_string(),
        )
    };
    stages.push(Stage::Draft);
    tracing::info!(task = %task.id, ?status, files = changed.len(), "coding task drafted");
    Ok(CodingResult {
        task: task.id,
        status,
        failure_class: failure,
        stages,
        base_head,
        branch: wt.branch,
        worktree: wt.path,
        plan,
        changed_files: changed,
        diff,
        commands,
        review,
        summary,
    })
}

/// Removes the task worktree (the branch and its commits-to-be remain for review).
pub async fn discard_workspace(
    gate: &Gate,
    policy: PolicyContext,
    task: &CodingTask,
    result: &CodingResult,
) -> Result<()> {
    let cfg = RepoConfig::load(&task.repo)?;
    let runner = Runner::new(
        gate,
        RunnerSetup {
            task: task.id,
            trace: task.trace,
            ctx: policy,
            home: task.workspaces_root.join(format!(".home-{}", task.id)),
            timeout: Duration::from_secs(DISCARD_TIMEOUT_SECS),
            passthrough: Vec::new(),
            data_class: cfg.data_class,
        },
    );
    worktree::remove(&runner, &task.repo, &result.worktree).await
}
