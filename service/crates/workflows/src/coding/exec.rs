//! Process execution shared by every sandbox: the child leads its own process group, output
//! is streamed into bounded tail buffers (never unbounded), and on timeout, exit or drop the
//! WHOLE group is killed, grandchildren included.
use async_trait::async_trait;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

const READ_CHUNK_BYTES: usize = 8 * 1024;
/// How long to wait for the output readers after the process group is dead. Only a process
/// that escaped the group (setsid) can still hold the pipes open.
const READER_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The complete environment of the process; nothing is inherited.
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    /// Each of stdout and stderr keeps at most this many trailing bytes.
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The process could not be started (not found, not executable, no such directory).
    pub spawn_error: Option<String>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Bytes the process wrote, including those dropped by the cap.
    pub stdout_total: u64,
    pub stderr_total: u64,
}

impl ExecOutput {
    pub fn truncated(&self) -> bool {
        self.stdout_total > self.stdout.len() as u64 || self.stderr_total > self.stderr.len() as u64
    }
}

/// Runs one process to completion. Failures to start are reported in the output.
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    async fn exec(&self, spec: &ExecSpec) -> ExecOutput;
}

/// Sends SIGKILL to every process in the group led by `pid`. `kill -- -PID` is used instead of
/// `killpg(2)` to keep this crate free of `unsafe`.
fn kill_group(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(e) = status {
        tracing::warn!(pid, error = %e, "could not signal process group");
    }
}

/// Kills the process group when dropped, so a cancelled future cannot leak children.
struct GroupGuard(Option<u32>);

impl GroupGuard {
    fn kill(&self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Reads to EOF keeping only the last `cap` bytes; memory never exceeds `2 * cap` plus a chunk.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, cap: usize) -> (Vec<u8>, u64) {
    let Some(mut reader) = reader else {
        return (Vec::new(), 0);
    };
    let (mut buf, mut total) = (Vec::new(), 0u64);
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total = total.saturating_add(n as u64);
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > cap.saturating_mul(2) {
                    buf.drain(..buf.len() - cap);
                }
            }
        }
    }
    if buf.len() > cap {
        buf.drain(..buf.len() - cap);
    }
    (buf, total)
}

async fn join_reader(mut handle: tokio::task::JoinHandle<(Vec<u8>, u64)>) -> (Vec<u8>, u64) {
    match timeout(READER_GRACE, &mut handle).await {
        Ok(Ok(v)) => v,
        _ => {
            handle.abort();
            (Vec::new(), 0)
        }
    }
}

/// Real process execution on this machine.
pub struct ProcessExecutor;

#[async_trait]
impl CommandExecutor for ProcessExecutor {
    async fn exec(&self, spec: &ExecSpec) -> ExecOutput {
        let mut out = ExecOutput::default();
        let Some((program, args)) = spec.argv.split_first() else {
            out.spawn_error = Some("empty command".into());
            return out;
        };
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                out.spawn_error = Some(e.to_string());
                return out;
            }
        };
        let guard = GroupGuard(child.id());
        let cap = spec.max_output_bytes;
        let stdout = tokio::spawn(read_capped(child.stdout.take(), cap));
        let stderr = tokio::spawn(read_capped(child.stderr.take(), cap));
        match timeout(spec.timeout, child.wait()).await {
            Ok(Ok(status)) => out.exit_code = status.code(),
            Ok(Err(e)) => out.spawn_error = Some(e.to_string()),
            Err(_) => {
                out.timed_out = true;
                guard.kill();
                if let Err(e) = child.wait().await {
                    tracing::warn!(error = %e, "could not reap timed-out child");
                }
            }
        }
        // Background children of a finished command would otherwise keep running and keep
        // the pipes open.
        guard.kill();
        (out.stdout, out.stdout_total) = join_reader(stdout).await;
        (out.stderr, out.stderr_total) = join_reader(stderr).await;
        out
    }
}
