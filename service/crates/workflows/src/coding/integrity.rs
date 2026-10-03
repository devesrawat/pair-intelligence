//! Tamper detection for the ORIGINAL repository. A content hash over everything in its git
//! directory that can change what runs or what a ref points to: `config`, `hooks/**`, `HEAD`,
//! `packed-refs`, `refs/**` and `info/{attributes,exclude}`. Computed with plain file reads, so
//! a poisoned config is never handed to git while checking. The task's own branch is excluded
//! because creating the worktree legitimately adds it.
use pair_core::error::{ErrorCode, PairError, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
};

const FILES: [&str; 6] = [
    "config",
    "config.worktree",
    "HEAD",
    "packed-refs",
    "info/attributes",
    "info/exclude",
];
const TREES: [&str; 2] = ["hooks", "refs"];

fn io_err(path: &Path, e: &io::Error) -> PairError {
    PairError::new(
        ErrorCode::Internal,
        format!("integrity check {}: {e}", path.display()),
    )
}

/// The git directory (and the common directory when the repo is itself a linked worktree).
fn git_dirs(repo: &Path) -> Result<Vec<PathBuf>> {
    let dot_git = repo.join(".git");
    let meta = fs::symlink_metadata(&dot_git).map_err(|e| io_err(&dot_git, &e))?;
    if meta.is_dir() {
        return Ok(vec![dot_git]);
    }
    let raw = fs::read_to_string(&dot_git).map_err(|e| io_err(&dot_git, &e))?;
    let target = raw
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .ok_or_else(|| PairError::new(ErrorCode::Internal, "repo .git has no gitdir pointer"))?;
    let git_dir = repo.join(target);
    let mut dirs = vec![git_dir.clone()];
    if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
        dirs.push(git_dir.join(common.trim()));
    }
    Ok(dirs)
}

fn mode_bits(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn absorb_file(h: &mut Sha256, rel: &str, path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(path, &e)),
    };
    let content = if meta.file_type().is_symlink() {
        fs::read_link(path)
            .map_err(|e| io_err(path, &e))?
            .display()
            .to_string()
            .into_bytes()
    } else {
        fs::read(path).map_err(|e| io_err(path, &e))?
    };
    h.update(rel.as_bytes());
    h.update([0]);
    h.update(mode_bits(&meta).to_be_bytes());
    h.update((content.len() as u64).to_be_bytes());
    h.update(&content);
    Ok(())
}

fn absorb_tree(h: &mut Sha256, rel: &str, dir: &Path, skip: &str) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(dir, &e)),
    };
    let mut names = entries
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(|e| io_err(dir, &e))?;
    names.sort();
    for name in names {
        let (child_rel, child) = (format!("{rel}/{name}"), dir.join(&name));
        if child_rel == skip {
            continue;
        }
        let meta = fs::symlink_metadata(&child).map_err(|e| io_err(&child, &e))?;
        if meta.is_dir() {
            absorb_tree(h, &child_rel, &child, skip)?;
        } else {
            absorb_file(h, &child_rel, &child)?;
        }
    }
    Ok(())
}

/// Hash of the repository's tamper-relevant git state; `skip_ref` (e.g. `refs/heads/pair/<id>`)
/// is left out.
pub(super) fn fingerprint(repo: &Path, skip_ref: &str) -> Result<String> {
    let mut h = Sha256::new();
    for (i, dir) in git_dirs(repo)?.iter().enumerate() {
        h.update(format!("dir{i}").as_bytes());
        for f in FILES {
            absorb_file(&mut h, f, &dir.join(f))?;
        }
        for t in TREES {
            absorb_tree(&mut h, t, &dir.join(t), skip_ref)?;
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
