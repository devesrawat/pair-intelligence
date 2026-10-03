//! Remote writes (push, PR) are never executed by this crate. They resolve to an approval
//! request bound to ONE hash: the one the policy computes for the exact `ActionRequest` that
//! executing the write will later present to `Gate::execute`. The request is built by the same
//! function the `Runner` uses (`action_request`), from the real `git push` argv, so what an
//! approver binds to is what runs. The egress host is derived from the remote URL itself, never
//! supplied separately.
use super::runner::{action_request, Runner};
use crate::tools::{GIT_PUSH, PR_CREATE};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{TaskId, TraceId},
    traits::Policy,
    types::{ActionRequest, DataClass, Decision, PolicyContext},
};
use pair_policy::{config::ActionClass, payload::payload_hash};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MIN_SHA_LEN: usize = 40;
const MAX_SHA_LEN: usize = 64;
const MAX_BRANCH_LEN: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKind {
    Push,
    OpenPr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAction {
    pub kind: RemoteKind,
    /// Push URL of the remote (`https://`, `ssh://` or scp-style `git@host:path`). Remote
    /// names are refused: the egress host must be derivable from what is actually contacted.
    /// Resolve a name with [`resolve_push_url`].
    pub remote_url: String,
    pub branch: String,
    /// Exact commit to publish; the push refspec is `<head_sha>:refs/heads/<branch>`.
    pub head_sha: String,
    /// Hash of the PR content (title, body, diff); part of the approved payload for PRs.
    pub diff_sha256: String,
    /// Declared class of the data being sent (from the repo config).
    pub data_class: DataClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOutcome {
    NeedsApproval {
        payload_hash: String,
    },
    /// Policy allowed it AND an approval id was supplied. The caller must still
    /// consume the approval against `payload_hash` before executing.
    Authorized {
        payload_hash: String,
    },
}

/// Who asks: the policy engine, its context (approval ids offered live in `ctx.approvals`),
/// the task identity and the directory the write will execute in.
pub struct RemoteCtx<'a> {
    pub policy: &'a dyn Policy,
    pub ctx: &'a PolicyContext,
    pub task: TaskId,
    pub trace: TraceId,
    pub cwd: &'a Path,
}

fn denied(msg: impl Into<String>) -> PairError {
    PairError::new(ErrorCode::PolicyDenied, msg)
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && !host.contains("..")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// The host a remote URL connects to, lowercased. Accepts `https://[user@]host[:port]/path`,
/// `ssh://[user@]host[:port]/path` and scp-style `[user@]host:path`; anything else, and any
/// embedded password, is refused.
pub fn egress_host(remote_url: &str) -> Result<String> {
    let url = remote_url.trim();
    let authority_and_path = match url.split_once("://") {
        Some((scheme, rest)) if matches!(scheme.to_ascii_lowercase().as_str(), "https" | "ssh") => {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            rest[..end].to_string()
        }
        Some((scheme, _)) => {
            return Err(denied(format!("remote scheme {scheme:?} is not allowed")))
        }
        None => {
            // scp-style: `host:path`, the host part may not contain a slash
            let (host_part, _) = url
                .split_once(':')
                .filter(|(h, _)| !h.contains('/') && !h.is_empty())
                .ok_or_else(|| {
                    denied("remote must be a URL (https, ssh or scp-style); names are not accepted")
                })?;
            host_part.to_string()
        }
    };
    let host_port = match authority_and_path.rsplit_once('@') {
        Some((userinfo, hp)) if !userinfo.contains(':') => hp,
        Some(_) => return Err(denied("remote URL must not embed a password")),
        None => authority_and_path.as_str(),
    };
    let host = match host_port.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        Some(_) => return Err(denied("remote URL has an invalid port")),
        None => host_port,
    }
    .to_ascii_lowercase();
    if valid_host(&host) {
        Ok(host)
    } else {
        Err(denied(format!(
            "remote host {host:?} is not a valid hostname"
        )))
    }
}

fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= MAX_BRANCH_LEN
        && !b.starts_with(['-', '/', '.'])
        && !b.ends_with(['/', '.'])
        && !b.contains("..")
        && !b.contains("//")
        && !b.ends_with(".lock")
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

impl RemoteAction {
    fn validate(&self) -> Result<()> {
        if !valid_branch(&self.branch) {
            return Err(denied(format!("invalid branch name {:?}", self.branch)));
        }
        let sha_ok = (MIN_SHA_LEN..=MAX_SHA_LEN).contains(&self.head_sha.len())
            && self.head_sha.chars().all(|c| c.is_ascii_hexdigit());
        if !sha_ok {
            return Err(denied("head_sha must be a full commit id"));
        }
        Ok(())
    }

    /// Egress host, derived from `remote_url`.
    pub fn host(&self) -> Result<String> {
        egress_host(&self.remote_url)
    }

    /// The real command a push runs: `git push <url> <sha>:refs/heads/<branch>`.
    pub fn push_argv(&self) -> Vec<String> {
        vec![
            "git".into(),
            "push".into(),
            self.remote_url.clone(),
            format!("{}:refs/heads/{}", self.head_sha, self.branch),
        ]
    }

    fn tool(&self) -> &'static str {
        match self.kind {
            RemoteKind::Push => GIT_PUSH,
            RemoteKind::OpenPr => PR_CREATE,
        }
    }

    /// The exact request the policy hashes, now and when the write executes. For a push it is
    /// identical to what `Runner::run_tool(GIT_PUSH, push_argv, cwd, Some(host))` presents.
    pub fn action_request(
        &self,
        task: TaskId,
        trace: TraceId,
        cwd: &Path,
    ) -> Result<ActionRequest> {
        self.validate()?;
        let host = self.host()?;
        match self.kind {
            RemoteKind::Push => action_request(
                task,
                trace,
                self.data_class,
                GIT_PUSH,
                &self.push_argv(),
                cwd,
                Some(&host),
            ),
            RemoteKind::OpenPr => Ok(ActionRequest {
                tool: self.tool().to_string(),
                executable: None,
                args: vec![
                    "pr".into(),
                    "create".into(),
                    self.remote_url.clone(),
                    self.branch.clone(),
                    self.head_sha.clone(),
                    self.diff_sha256.clone(),
                ],
                paths: vec![cwd.display().to_string()],
                destination: Some(host),
                data_class: self.data_class,
                task,
                trace,
            }),
        }
    }
}

/// The hash an approver must bind to; it equals what the Gate recomputes at execution.
pub fn request_remote_write(cx: &RemoteCtx<'_>, action: &RemoteAction) -> Result<RemoteOutcome> {
    let req = action.action_request(cx.task, cx.trace, cx.cwd)?;
    match cx.policy.authorize(&req, cx.ctx).decision {
        Decision::Deny { reason } => Err(PairError::new(ErrorCode::PolicyDenied, reason)),
        Decision::NeedsApproval { payload_hash } => {
            Ok(RemoteOutcome::NeedsApproval { payload_hash })
        }
        // Defence in depth: an Allow without any approval id is still not enough. The hash is
        // the one the Gate computes for an external write of this exact request.
        Decision::Allow => {
            let payload_hash = payload_hash(&req, ActionClass::ExternalWrite)
                .map_err(|e| PairError::new(ErrorCode::Internal, e.to_string()))?;
            Ok(if cx.ctx.approvals.is_empty() {
                RemoteOutcome::NeedsApproval { payload_hash }
            } else {
                RemoteOutcome::Authorized { payload_hash }
            })
        }
    }
}

/// `git remote get-url --push <name>` in `repo`: the URL a push to `name` really contacts.
pub async fn resolve_push_url(runner: &Runner<'_>, repo: &Path, name: &str) -> Result<String> {
    if name.is_empty() || name.starts_with('-') {
        return Err(denied("invalid remote name"));
    }
    let argv: Vec<String> = ["git", "remote", "get-url", "--push", name]
        .map(String::from)
        .to_vec();
    let report = runner.run_control(&argv, repo, None).await?;
    if !report.passed() {
        return Err(PairError::new(
            ErrorCode::NotFound,
            format!("remote {name:?}: {}", report.stderr.trim()),
        ));
    }
    let url = report.stdout.trim().to_string();
    egress_host(&url)?;
    Ok(url)
}
