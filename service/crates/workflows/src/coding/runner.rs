//! Gate-routed command execution with a scrubbed environment. Nothing is spawned outside the
//! `Gate::execute` closure, i.e. before `Allow` or a consumed matching approval.
//!
//! Two paths exist, and only the first ever sees model-influenced input:
//! * [`Runner::run`] / [`Runner::run_tool`] hand the command to the [`Sandbox`] (a container
//!   by default). Build, test and acceptance commands, which execute model-written code, go
//!   here, and each one counts against the run's tool-call limit.
//! * `run_control` (crate-private) is PAIR's own fixed-argv git plumbing on the task
//!   worktree (`worktree add`, `diff`, ...). It runs on the host, hardened against
//!   repo-controlled config (see `control_env`), never takes argv from a model or a repo
//!   config, and is not reachable from outside `coding`.
use super::{
    exec::{CommandExecutor, ExecOutput, ExecSpec, ProcessExecutor},
    sandbox::{ContainerSandbox, Sandbox},
};
use crate::{limits::RunLimits, tools::SHELL_EXEC};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{TaskId, TraceId},
    types::{ActionRequest, DataClass, PolicyContext},
};
use pair_policy::Gate;
use serde::{Deserialize, Serialize};
use std::{path::Path, path::PathBuf, sync::Arc, time::Duration, time::Instant};

pub(super) const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const MIN_REDACTABLE_LEN: usize = 6;
const REDACTED: &str = "[REDACTED]";
const SECRET_NAME_MARKERS: [&str; 8] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
    "COOKIE",
];

/// True for environment variable names that look like credentials.
pub fn looks_secret_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    SECRET_NAME_MARKERS.iter().any(|m| up.contains(m))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdReport {
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Set when the process could not be started (not found, not executable, no container).
    pub spawn_error: Option<String>,
    pub stdout: String,
    pub stderr: String,
    /// More output was produced than the capped tail kept in `stdout` / `stderr`.
    pub output_truncated: bool,
    pub duration_ms: u64,
}

impl CmdReport {
    pub fn passed(&self) -> bool {
        !self.timed_out && self.spawn_error.is_none() && self.exit_code == Some(0)
    }
}

/// Everything a `Runner` needs besides the gate.
pub struct RunnerSetup {
    pub task: TaskId,
    pub trace: TraceId,
    /// Active policy version, the policy workspace root and the approval ids offered for
    /// this task (normally empty).
    pub ctx: PolicyContext,
    /// An empty directory used as HOME so user-level credential stores (~/.aws, ~/.ssh,
    /// ~/.config/gh) are unreachable from the command.
    pub home: PathBuf,
    pub timeout: Duration,
    /// Extra environment variable names forwarded to commands.
    pub passthrough: Vec<String>,
    /// Declared class of the data the commands touch (from the repo config).
    pub data_class: DataClass,
}

/// Pins git to one git directory and work tree, ignoring any `.git` file in the work tree.
#[derive(Debug, Clone)]
pub(super) struct GitPin {
    pub git_dir: PathBuf,
    pub work_tree: PathBuf,
}

pub struct Runner<'a> {
    gate: &'a Gate,
    task: TaskId,
    trace: TraceId,
    ctx: PolicyContext,
    timeout: Duration,
    home: PathBuf,
    passthrough: Vec<String>,
    data_class: DataClass,
    redactions: Vec<String>,
    sandbox: Arc<dyn Sandbox>,
    control: Arc<dyn CommandExecutor>,
    limits: Arc<RunLimits>,
}

/// The ActionRequest for one command. Shared with `remote` so an approval hash computed
/// before a push equals the one the Gate computes when the push really runs.
pub(super) fn action_request(
    task: TaskId,
    trace: TraceId,
    data_class: DataClass,
    tool: &str,
    argv: &[String],
    cwd: &Path,
    destination: Option<&str>,
) -> Result<ActionRequest> {
    let Some((exe, args)) = argv.split_first() else {
        return Err(PairError::new(ErrorCode::InvalidInput, "empty command"));
    };
    Ok(ActionRequest {
        tool: tool.to_string(),
        executable: Some(exe.clone()),
        args: args.to_vec(),
        paths: vec![cwd.display().to_string()],
        destination: destination.map(str::to_string),
        data_class,
        task,
        trace,
    })
}

impl<'a> Runner<'a> {
    /// Commands run in the container sandbox (image from `PAIR_WORKER_IMAGE`, refused when
    /// unset) with the default per-run limits. Use [`Runner::with_sandbox`] to inject another.
    pub fn new(gate: &'a Gate, setup: RunnerSetup) -> Self {
        let RunnerSetup {
            task,
            trace,
            ctx,
            home,
            timeout,
            passthrough,
            data_class,
        } = setup;
        let redactions = std::env::vars()
            .filter(|(k, v)| looks_secret_name(k) && v.len() >= MIN_REDACTABLE_LEN)
            .map(|(_, v)| v)
            .collect();
        let exec: Arc<dyn CommandExecutor> = Arc::new(ProcessExecutor);
        Self {
            gate,
            task,
            trace,
            ctx,
            timeout,
            home,
            passthrough,
            data_class,
            redactions,
            sandbox: Arc::new(ContainerSandbox::from_env(exec.clone())),
            control: exec,
            limits: Arc::new(RunLimits::interactive()),
        }
    }

    #[must_use]
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: Arc<RunLimits>) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_control_executor(mut self, control: Arc<dyn CommandExecutor>) -> Self {
        self.control = control;
        self
    }

    pub fn sandbox_name(&self) -> &'static str {
        self.sandbox.name()
    }

    fn request(
        &self,
        tool: &str,
        argv: &[String],
        cwd: &Path,
        destination: Option<&str>,
    ) -> Result<ActionRequest> {
        action_request(
            self.task,
            self.trace,
            self.data_class,
            tool,
            argv,
            cwd,
            destination,
        )
    }

    /// Logical environment of a command. `HOME` and `PATH` are the host's; the container
    /// sandbox replaces both.
    fn scrubbed_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("HOME".to_string(), self.home.display().to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ];
        for name in std::iter::once("PATH").chain(self.passthrough.iter().map(String::as_str)) {
            if let Ok(v) = std::env::var(name) {
                env.push((name.to_string(), v));
            }
        }
        env
    }

    /// Environment of control git: repo-level config cannot start programs (hooks, fsmonitor,
    /// external transports), and git is pinned to the git directory resolved at worktree
    /// creation, so a `.git` file rewritten from inside the sandbox is never followed.
    fn control_env(&self, pin: Option<&GitPin>) -> Vec<(String, String)> {
        let mut env = self.scrubbed_env();
        for (k, v) in [
            ("GIT_CONFIG_COUNT", "3"),
            ("GIT_CONFIG_KEY_0", "core.hooksPath"),
            ("GIT_CONFIG_VALUE_0", "/dev/null"),
            ("GIT_CONFIG_KEY_1", "core.fsmonitor"),
            ("GIT_CONFIG_VALUE_1", "false"),
            ("GIT_CONFIG_KEY_2", "protocol.ext.allow"),
            ("GIT_CONFIG_VALUE_2", "never"),
            ("GIT_NO_LAZY_FETCH", "1"),
        ] {
            env.push((k.to_string(), v.to_string()));
        }
        if let Some(p) = pin {
            env.push(("GIT_DIR".into(), p.git_dir.display().to_string()));
            env.push(("GIT_WORK_TREE".into(), p.work_tree.display().to_string()));
        }
        env
    }

    fn redact(&self, bytes: &[u8]) -> String {
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        for secret in &self.redactions {
            text = text.replace(secret.as_str(), REDACTED);
        }
        text
    }

    fn report(&self, argv: &[String], out: ExecOutput, started: Instant) -> CmdReport {
        let report = CmdReport {
            argv: argv.to_vec(),
            exit_code: out.exit_code,
            timed_out: out.timed_out,
            spawn_error: out.spawn_error.clone(),
            stdout: self.redact(&out.stdout),
            stderr: self.redact(&out.stderr),
            output_truncated: out.truncated(),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        tracing::info!(task = %self.task, exe = %argv[0], exit = ?report.exit_code, timed_out = report.timed_out, "command finished");
        report
    }

    /// Runs through `Gate::execute`; the spawn closure is unreachable without Allow or a
    /// consumed approval. Spawn failures and timeouts are reported in the `CmdReport`, not as
    /// `Err`, so callers can classify them.
    pub async fn run(&self, argv: &[String], cwd: &Path) -> Result<CmdReport> {
        self.run_tool(SHELL_EXEC, argv, cwd, None).await
    }

    /// Like `run` but under another registered tool (e.g. `git.push`) with the declared
    /// egress `destination`. External-write tools execute only if the Gate consumes one of
    /// the approvals in the context for this exact request. The command runs in the sandbox,
    /// which has no network: remote writes are executed by the caller with an
    /// egress-capable sandbox after the approval is consumed.
    pub async fn run_tool(
        &self,
        tool: &str,
        argv: &[String],
        cwd: &Path,
        destination: Option<&str>,
    ) -> Result<CmdReport> {
        let req = self.request(tool, argv, cwd, destination)?;
        self.limits.begin_tool_call()?;
        self.gate
            .execute(&req, &self.ctx, || self.spawn(argv, cwd))
            .await
    }

    async fn spawn(&self, argv: &[String], cwd: &Path) -> Result<CmdReport> {
        let started = Instant::now();
        let spec = ExecSpec {
            argv: argv.to_vec(),
            cwd: cwd.to_path_buf(),
            env: self.scrubbed_env(),
            timeout: self.timeout.min(self.limits.remaining()?),
            max_output_bytes: MAX_OUTPUT_BYTES,
        };
        let out = self.sandbox.run(&spec).await?;
        Ok(self.report(argv, out, started))
    }

    /// PAIR-authored git plumbing, host side. Policy-gated like every command.
    pub(super) async fn run_control(
        &self,
        argv: &[String],
        cwd: &Path,
        pin: Option<&GitPin>,
    ) -> Result<CmdReport> {
        let req = self.request(SHELL_EXEC, argv, cwd, None)?;
        self.gate
            .execute(&req, &self.ctx, || async {
                let started = Instant::now();
                let spec = ExecSpec {
                    argv: argv.to_vec(),
                    cwd: cwd.to_path_buf(),
                    env: self.control_env(pin),
                    timeout: self.timeout.min(self.limits.remaining()?),
                    max_output_bytes: MAX_DIFF_BYTES,
                };
                let out = self.control.exec(&spec).await;
                Ok(self.report(argv, out, started))
            })
            .await
    }
}

/// Control git output (diffs, file lists) is larger than command output.
const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;
