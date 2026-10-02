//! One git worktree + branch per task, driven through the git CLI via the policy-gated runner.
use super::{runner::Runner, scope::is_secret_path};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::TaskId,
};
use std::path::{Path, PathBuf};

const BRANCH_PREFIX: &str = "pair/";

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// Runs `git <args>` in `cwd`; non-zero exit or spawn failure is an error.
pub async fn git(runner: &Runner<'_>, cwd: &Path, args: &[&str]) -> Result<String> {
    let mut full = vec!["git"];
    full.extend_from_slice(args);
    let report = runner.run(&argv(&full), cwd).await?;
    if report.passed() {
        Ok(report.stdout)
    } else {
        Err(PairError::new(
            ErrorCode::Internal,
            format!(
                "git {} failed: {} {:?}",
                args.join(" "),
                report.stderr.trim(),
                report.spawn_error
            ),
        ))
    }
}

pub async fn head(runner: &Runner<'_>, repo: &Path) -> Result<String> {
    Ok(git(runner, repo, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string())
}

pub async fn create(
    runner: &Runner<'_>,
    repo: &Path,
    root: &Path,
    task: TaskId,
) -> Result<Worktree> {
    let path = root.join(task.to_string());
    let branch = format!("{BRANCH_PREFIX}{task}");
    let path_str = path.display().to_string();
    git(
        runner,
        repo,
        &["worktree", "add", "-b", &branch, &path_str, "HEAD"],
    )
    .await?;
    strip_secrets(runner, &path).await?;
    Ok(Worktree { path, branch })
}

/// Credential files tracked by the repo are removed from the workspace and hidden
/// from git so their absence never shows up in the patch.
async fn strip_secrets(runner: &Runner<'_>, wt: &Path) -> Result<()> {
    let listing = git(runner, wt, &["ls-files", "-z"]).await?;
    for file in listing
        .split('\0')
        .filter(|f| !f.is_empty() && is_secret_path(f))
    {
        git(runner, wt, &["update-index", "--skip-worktree", "--", file]).await?;
        std::fs::remove_file(wt.join(file))
            .map_err(|e| PairError::new(ErrorCode::Internal, format!("strip {file}: {e}")))?;
        tracing::info!(file, "credential file withheld from workspace");
    }
    Ok(())
}

/// Stages everything and lists changed paths relative to HEAD.
pub async fn changed_files(runner: &Runner<'_>, wt: &Path) -> Result<Vec<String>> {
    git(runner, wt, &["add", "-A"]).await?;
    let out = git(
        runner,
        wt,
        &["diff", "--cached", "--name-only", "-z", "HEAD"],
    )
    .await?;
    Ok(out
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect())
}

pub async fn diff(runner: &Runner<'_>, wt: &Path) -> Result<String> {
    git(runner, wt, &["add", "-A"]).await?;
    git(runner, wt, &["diff", "--cached", "HEAD"]).await
}

pub async fn remove(runner: &Runner<'_>, repo: &Path, wt_path: &Path) -> Result<()> {
    let p = wt_path.display().to_string();
    git(runner, repo, &["worktree", "remove", "--force", &p]).await?;
    Ok(())
}
