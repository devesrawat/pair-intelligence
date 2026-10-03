//! Where model-influenced commands run. `ContainerSandbox` (the default) runs every command
//! in a throwaway container on the worker image with the profile of
//! `deploy/worker/compose.worker.yaml`: no network, read-only root, all capabilities
//! dropped, no-new-privileges, pid/memory limits, non-root, and ONLY the task worktree
//! mounted (read-write). The original repository, its `.git` and its object store are never
//! mounted, so nothing inside can read history (`git show HEAD:.env`) or write hooks/config.
//!
//! `HostSandbox` runs on the host and is refused unless `PAIR_ALLOW_HOST_EXEC=1`; it exists
//! for tests only.
use super::exec::{CommandExecutor, ExecOutput, ExecSpec};
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use std::{path::Path, sync::Arc, time::Duration};

pub const HOST_EXEC_ENV: &str = "PAIR_ALLOW_HOST_EXEC";
pub const WORKER_IMAGE_ENV: &str = "PAIR_WORKER_IMAGE";
const DOCKER_BIN: &str = "docker";
/// Host environment the docker CLI itself needs (never forwarded into the container).
const DOCKER_CLI_ENV: [&str; 5] = [
    "PATH",
    "HOME",
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_CONTEXT",
];
/// `docker run` exits with this when the daemon or the run itself failed (not the command).
const DOCKER_RUN_FAILED: i32 = 125;
const DOCKER_KILL_TIMEOUT: Duration = Duration::from_secs(15);
const DOCKER_KILL_OUTPUT_BYTES: usize = 4096;

// Mirrors deploy/worker/compose.worker.yaml; `compose_profile_matches_sandbox_args` fails on drift.
pub(super) const WORKER_USER: &str = "10001:10001";
pub(super) const PIDS_LIMIT: &str = "256";
pub(super) const MEMORY_LIMIT: &str = "1g";
pub(super) const CPUS: &str = "1.0";
pub(super) const TMPFS: &str = "/tmp:rw,noexec,nosuid,size=128m";
pub(super) const ULIMITS: [&str; 3] = ["nofile=1024:2048", "nproc=256", "fsize=1073741824"];
const CONTAINER_HOME: &str = "/tmp/home";

#[async_trait]
pub trait Sandbox: Send + Sync {
    /// `spec.cwd` is the task worktree; the process sees nothing else of the host.
    async fn run(&self, spec: &ExecSpec) -> Result<ExecOutput>;
    fn name(&self) -> &'static str;
}

/// Host execution. Constructing one without `PAIR_ALLOW_HOST_EXEC=1` fails.
pub struct HostSandbox {
    exec: Arc<dyn CommandExecutor>,
}

impl HostSandbox {
    pub fn from_env(exec: Arc<dyn CommandExecutor>) -> Result<Self> {
        Self::with_lookup(exec, |k| std::env::var(k).ok())
    }

    /// `lookup` stands in for the process environment so tests need no global state.
    pub fn with_lookup(
        exec: Arc<dyn CommandExecutor>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        if lookup(HOST_EXEC_ENV).as_deref() != Some("1") {
            return Err(PairError::new(
                ErrorCode::PolicyDenied,
                format!(
                    "host execution of model-influenced commands is refused; \
                     run them in the container sandbox (host exec needs {HOST_EXEC_ENV}=1, tests only)"
                ),
            ));
        }
        tracing::warn!(
            "HOST EXECUTION ENABLED via {HOST_EXEC_ENV}=1: model-written code will run on this host without isolation"
        );
        Ok(Self { exec })
    }
}

#[async_trait]
impl Sandbox for HostSandbox {
    async fn run(&self, spec: &ExecSpec) -> Result<ExecOutput> {
        Ok(self.exec.exec(spec).await)
    }
    fn name(&self) -> &'static str {
        "host"
    }
}

/// `docker run --rm` on the worker image, one container per command.
pub struct ContainerSandbox {
    exec: Arc<dyn CommandExecutor>,
    image: Option<String>,
}

impl ContainerSandbox {
    pub fn new(exec: Arc<dyn CommandExecutor>, image: Option<String>) -> Self {
        Self {
            exec,
            image: image.filter(|i| !i.trim().is_empty()),
        }
    }

    /// Image from `PAIR_WORKER_IMAGE`. Without one every run is refused (fail closed).
    pub fn from_env(exec: Arc<dyn CommandExecutor>) -> Self {
        Self::new(exec, std::env::var(WORKER_IMAGE_ENV).ok())
    }

    fn deny(msg: impl Into<String>) -> PairError {
        PairError::new(ErrorCode::PolicyDenied, msg)
    }

    /// The exact `docker run` argv for one command (without the leading `docker`).
    pub(super) fn run_args(image: &str, name: &str, spec: &ExecSpec) -> Result<Vec<String>> {
        let cwd = spec.cwd.as_path();
        let cwd_str = cwd
            .to_str()
            .filter(|p| cwd.is_absolute() && !p.contains([':', ',', '\n', '\0']))
            .ok_or_else(|| Self::deny(format!("unmountable sandbox directory {cwd:?}")))?;
        if cwd.join(".git").is_dir() {
            return Err(Self::deny(format!(
                "{cwd_str} holds a .git directory; a repository is never mounted into the sandbox"
            )));
        }
        let mut a: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--init".into(),
            "--name".into(),
            name.into(),
            "--network".into(),
            "none".into(),
            "--read-only".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges:true".into(),
            "--pids-limit".into(),
            PIDS_LIMIT.into(),
            "--memory".into(),
            MEMORY_LIMIT.into(),
            "--memory-swap".into(),
            MEMORY_LIMIT.into(),
            "--cpus".into(),
            CPUS.into(),
            "--user".into(),
            WORKER_USER.into(),
            "--tmpfs".into(),
            TMPFS.into(),
        ];
        for u in ULIMITS {
            a.extend(["--ulimit".into(), u.into()]);
        }
        a.extend([
            "-v".into(),
            format!("{cwd_str}:{cwd_str}:rw"),
            "-w".into(),
            cwd_str.into(),
            "-e".into(),
            format!("HOME={CONTAINER_HOME}"),
            "-e".into(),
            format!("PAIR_WORKSPACE_ROOT={cwd_str}"),
        ]);
        for (k, v) in &spec.env {
            // HOME and PATH are the host's; the container has its own.
            if k != "HOME" && k != "PATH" && k != "PAIR_WORKSPACE_ROOT" {
                a.extend(["-e".into(), format!("{k}={v}")]);
            }
        }
        a.push(image.into());
        a.extend(spec.argv.iter().cloned());
        Ok(a)
    }

    fn cli_spec(&self, args: Vec<String>, cwd: &Path, timeout: Duration) -> ExecSpec {
        let env = DOCKER_CLI_ENV
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
            .collect();
        let mut argv = vec![DOCKER_BIN.to_string()];
        argv.extend(args);
        ExecSpec {
            argv,
            cwd: cwd.to_path_buf(),
            env,
            timeout,
            max_output_bytes: DOCKER_KILL_OUTPUT_BYTES,
        }
    }
}

/// Kills the container if the run future is dropped before it finished.
struct KillOnDrop {
    name: String,
    armed: bool,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let spawned = std::process::Command::new(DOCKER_BIN)
                .args(["kill", &self.name])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Err(e) = spawned {
                tracing::error!(container = %self.name, error = %e, "could not kill abandoned container");
            }
        }
    }
}

#[async_trait]
impl Sandbox for ContainerSandbox {
    async fn run(&self, spec: &ExecSpec) -> Result<ExecOutput> {
        let image = self.image.as_deref().ok_or_else(|| {
            Self::deny(format!(
                "{WORKER_IMAGE_ENV} is not set: refusing to run commands without the container sandbox"
            ))
        })?;
        let name = format!("pair-cmd-{}", uuid::Uuid::new_v4().simple());
        let args = Self::run_args(image, &name, spec)?;
        let docker = self.cli_spec(args, &spec.cwd, spec.timeout);
        let docker = ExecSpec {
            max_output_bytes: spec.max_output_bytes,
            ..docker
        };
        let mut guard = KillOnDrop {
            name: name.clone(),
            armed: true,
        };
        let mut out = self.exec.exec(&docker).await;
        guard.armed = false;
        if out.timed_out {
            // Killing the docker CLI does not stop the container; kill it by name.
            let kill = self.cli_spec(vec!["kill".into(), name], &spec.cwd, DOCKER_KILL_TIMEOUT);
            let killed = self.exec.exec(&kill).await;
            if killed.exit_code != Some(0) {
                tracing::error!(?killed.exit_code, "docker kill after timeout failed");
            }
        }
        if out.exit_code == Some(DOCKER_RUN_FAILED) && out.spawn_error.is_none() {
            // The daemon or the run failed, not the command: an environment problem.
            out.spawn_error = Some(format!(
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            out.exit_code = None;
        }
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "container"
    }
}
