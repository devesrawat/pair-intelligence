//! Gate-routed command execution with a scrubbed environment. A process is only spawned
//! inside the `Gate::execute` closure, i.e. after `Allow` or a consumed matching approval.
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{TaskId, TraceId},
    types::{ActionRequest, DataClass, PolicyContext},
};
use pair_policy::Gate;
use serde::{Deserialize, Serialize};
use std::{path::Path, path::PathBuf, process::Stdio, time::Duration, time::Instant};
use tokio::process::Command;

const MAX_OUTPUT_BYTES: usize = 32 * 1024;
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
const POLICY_VERSION_FALLBACK: &str = "coding-workflow";
const SHELL_TOOL: &str = "shell";

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
    /// Set when the process could not be started (not found, not executable).
    pub spawn_error: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

impl CmdReport {
    pub fn passed(&self) -> bool {
        !self.timed_out && self.spawn_error.is_none() && self.exit_code == Some(0)
    }
}

pub struct Runner<'a> {
    gate: &'a Gate,
    task: TaskId,
    trace: TraceId,
    ctx: PolicyContext,
    timeout: Duration,
    home: PathBuf,
    passthrough: Vec<String>,
    redactions: Vec<String>,
}

impl<'a> Runner<'a> {
    /// `home` is an empty directory used as HOME so user-level credential stores
    /// (~/.aws, ~/.ssh, ~/.config/gh) are unreachable from the command.
    pub fn new(
        gate: &'a Gate,
        task: TaskId,
        trace: TraceId,
        workspace_root: &Path,
        home: PathBuf,
        timeout: Duration,
        passthrough: Vec<String>,
    ) -> Self {
        let redactions = std::env::vars()
            .filter(|(k, v)| looks_secret_name(k) && v.len() >= MIN_REDACTABLE_LEN)
            .map(|(_, v)| v)
            .collect();
        let ctx = PolicyContext {
            workspace_root: workspace_root.display().to_string(),
            approvals: Vec::new(),
            policy_version: POLICY_VERSION_FALLBACK.to_string(),
        };
        Self {
            gate,
            task,
            trace,
            ctx,
            timeout,
            home,
            passthrough,
            redactions,
        }
    }

    fn request(&self, argv: &[String], cwd: &Path) -> Result<ActionRequest> {
        let Some((exe, args)) = argv.split_first() else {
            return Err(PairError::new(ErrorCode::InvalidInput, "empty command"));
        };
        Ok(ActionRequest {
            tool: SHELL_TOOL.to_string(),
            executable: Some(exe.clone()),
            args: args.to_vec(),
            paths: vec![cwd.display().to_string()],
            destination: None,
            data_class: DataClass::Personal,
            task: self.task,
            trace: self.trace,
        })
    }

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

    fn redact(&self, bytes: &[u8]) -> String {
        let start = bytes.len().saturating_sub(MAX_OUTPUT_BYTES);
        let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
        for secret in &self.redactions {
            text = text.replace(secret.as_str(), REDACTED);
        }
        text
    }

    /// Runs through `Gate::execute`; the spawn closure is unreachable without Allow or a
    /// consumed approval. Spawn failures and timeouts are reported in the `CmdReport`, not as
    /// `Err`, so callers can classify them.
    pub async fn run(&self, argv: &[String], cwd: &Path) -> Result<CmdReport> {
        let req = self.request(argv, cwd)?;
        self.gate
            .execute(&req, &self.ctx, || self.spawn(argv, cwd))
            .await
    }

    async fn spawn(&self, argv: &[String], cwd: &Path) -> Result<CmdReport> {
        let started = Instant::now();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(cwd)
            .env_clear()
            .envs(self.scrubbed_env())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let mut report = CmdReport {
            argv: argv.to_vec(),
            exit_code: None,
            timed_out: false,
            spawn_error: None,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
        };
        match tokio::time::timeout(self.timeout, cmd.output()).await {
            Err(_) => report.timed_out = true,
            Ok(Err(e)) => report.spawn_error = Some(e.to_string()),
            Ok(Ok(out)) => {
                report.exit_code = out.status.code();
                report.stdout = self.redact(&out.stdout);
                report.stderr = self.redact(&out.stderr);
            }
        }
        report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(task = %self.task, exe = %argv[0], exit = ?report.exit_code, timed_out = report.timed_out, "command finished");
        Ok(report)
    }
}
