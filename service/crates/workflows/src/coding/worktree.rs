//! One git worktree + branch per task, driven through the git CLI via the policy-gated runner.
//!
//! All git here is PAIR's own fixed-argv plumbing, run on the host through
//! `Runner::run_control`. The worktree is where model-written code runs (inside the sandbox),
//! so after creation git is pinned (`GIT_DIR`/`GIT_WORK_TREE`) to the git directory resolved
//! while the worktree was still pristine: a `.git` file rewritten by the sandboxed code is
//! never followed. The sandbox sees only the worktree directory, never the git directory.
use super::{
    runner::{GitPin, Runner},
    scope::is_secret_path,
};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::TaskId,
};
use std::path::{Path, PathBuf};

pub(super) const BRANCH_PREFIX: &str = "pair/";
const GITDIR_PREFIX: &str = "gitdir:";
/// Diffs must not invoke external diff or textconv programs.
const NO_EXT_DIFF: [&str; 2] = ["--no-ext-diff", "--no-textconv"];

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub(super) pin: GitPin,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// Runs `git <args>`; non-zero exit, spawn failure or truncated output is an error.
async fn git_in(
    runner: &Runner<'_>,
    cwd: &Path,
    pin: Option<&GitPin>,
    args: &[&str],
) -> Result<String> {
    let mut full = vec!["git"];
    full.extend_from_slice(args);
    let report = runner.run_control(&argv(&full), cwd, pin).await?;
    if report.passed() && !report.output_truncated {
        Ok(report.stdout)
    } else {
        Err(PairError::new(
            ErrorCode::Internal,
            format!(
                "git {} failed: {} {:?}{}",
                args.join(" "),
                report.stderr.trim(),
                report.spawn_error,
                if report.output_truncated {
                    " (output too large)"
                } else {
                    ""
                }
            ),
        ))
    }
}

pub async fn head(runner: &Runner<'_>, repo: &Path) -> Result<String> {
    Ok(git_in(runner, repo, None, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string())
}

/// Reads the `gitdir:` pointer git wrote into a fresh worktree.
fn resolve_gitdir(wt: &Path) -> Result<PathBuf> {
    let pointer = wt.join(".git");
    let raw = std::fs::read_to_string(&pointer)
        .map_err(|e| PairError::new(ErrorCode::Internal, format!("read {pointer:?}: {e}")))?;
    let target = raw
        .lines()
        .find_map(|l| l.strip_prefix(GITDIR_PREFIX))
        .map(str::trim)
        .ok_or_else(|| PairError::new(ErrorCode::Internal, "worktree .git has no gitdir"))?;
    let dir = wt.join(target);
    if !dir.is_dir() {
        return Err(PairError::new(
            ErrorCode::Internal,
            format!("worktree gitdir {dir:?} is not a directory"),
        ));
    }
    Ok(dir)
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
    git_in(
        runner,
        repo,
        None,
        &["worktree", "add", "-b", &branch, &path_str, "HEAD"],
    )
    .await?;
    let pin = GitPin {
        git_dir: resolve_gitdir(&path)?,
        work_tree: path.clone(),
    };
    strip_secrets(runner, &path, &pin).await?;
    Ok(Worktree { path, branch, pin })
}

/// Credential files tracked by the repo are removed from the workspace and hidden
/// from git so their absence never shows up in the patch. The sandbox never sees the
/// object store, so the committed copy is unreachable from model-written code.
async fn strip_secrets(runner: &Runner<'_>, wt: &Path, pin: &GitPin) -> Result<()> {
    let listing = git_in(runner, wt, Some(pin), &["ls-files", "-z"]).await?;
    for file in listing
        .split('\0')
        .filter(|f| !f.is_empty() && is_secret_path(f))
    {
        git_in(
            runner,
            wt,
            Some(pin),
            &["update-index", "--skip-worktree", "--", file],
        )
        .await?;
        std::fs::remove_file(wt.join(file))
            .map_err(|e| PairError::new(ErrorCode::Internal, format!("strip {file}: {e}")))?;
        tracing::info!(file, "credential file withheld from workspace");
    }
    Ok(())
}

/// Stages everything and lists changed paths relative to HEAD.
pub async fn changed_files(runner: &Runner<'_>, wt: &Worktree) -> Result<Vec<String>> {
    git_in(runner, &wt.path, Some(&wt.pin), &["add", "-A"]).await?;
    let mut args = vec!["diff", "--cached", "--name-only", "-z"];
    args.extend(NO_EXT_DIFF);
    args.push("HEAD");
    let out = git_in(runner, &wt.path, Some(&wt.pin), &args).await?;
    Ok(out
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect())
}

pub async fn diff(runner: &Runner<'_>, wt: &Worktree) -> Result<String> {
    git_in(runner, &wt.path, Some(&wt.pin), &["add", "-A"]).await?;
    let mut args = vec!["diff", "--cached"];
    args.extend(NO_EXT_DIFF);
    args.push("HEAD");
    git_in(runner, &wt.path, Some(&wt.pin), &args).await
}

pub async fn remove(runner: &Runner<'_>, repo: &Path, wt_path: &Path) -> Result<()> {
    let p = wt_path.display().to_string();
    let removed = git_in(runner, repo, None, &["worktree", "remove", "--force", &p]).await;
    if removed.is_ok() {
        return Ok(());
    }
    // A worktree whose .git pointer was rewritten fails validation; remove it by hand.
    std::fs::remove_dir_all(wt_path)
        .map_err(|e| PairError::new(ErrorCode::Internal, format!("remove {wt_path:?}: {e}")))?;
    git_in(runner, repo, None, &["worktree", "prune"]).await?;
    Ok(())
}
