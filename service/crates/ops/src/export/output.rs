//! The export directory: private (0700), files private (0600), never following a symlink, and
//! never mixing with foreign files.
//!
//! An export holds the owner's conversations and memories, so it must not be world-readable, and
//! `--out` is attacker-influenceable enough (a shared temp directory, a planted link) that every
//! file is created with `create_new` (`O_CREAT | O_EXCL`, which refuses an existing path including
//! a symlink, dangling or not) instead of being opened for overwrite.
use super::{Manifest, MANIFEST_FILE};
use crate::error::{OpsError, Result};
use std::fs::{DirBuilder, FileType, Metadata};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

fn refuse(path: &Path, why: &str) -> OpsError {
    OpsError::InvalidArgument(format!(
        "refusing export directory {}: {why}",
        path.display()
    ))
}

/// A validated, empty, private output directory.
pub(super) struct OutDir {
    path: PathBuf,
}

impl OutDir {
    /// Creates `path` (0700, parents too), or adopts an existing directory that is empty or holds
    /// only the files of an earlier export (those are removed, so nothing stale survives).
    pub(super) async fn prepare(path: &Path) -> Result<Self> {
        let owned = path.to_path_buf();
        tokio::task::spawn_blocking(move || prepare_blocking(&owned))
            .await
            .map_err(|e| OpsError::io(path, std::io::Error::other(e)))??;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Writes a new file; fails if anything already exists at that name.
    pub(super) async fn write(&self, name: &str, text: &str) -> Result<()> {
        let path = self.path.join(name);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&path)
            .await
            .map_err(|e| OpsError::io(&path, e))?;
        file.write_all(text.as_bytes())
            .await
            .map_err(|e| OpsError::io(&path, e))?;
        file.flush().await.map_err(|e| OpsError::io(&path, e))
    }
}

fn prepare_blocking(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => adopt_existing(path, &meta)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(path)
                .map_err(|e| OpsError::io(path, e))?;
        }
        Err(e) => return Err(OpsError::io(path, e)),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(DIR_MODE))
        .map_err(|e| OpsError::io(path, e))
}

fn adopt_existing(path: &Path, meta: &Metadata) -> Result<()> {
    let kind: FileType = meta.file_type();
    if kind.is_symlink() {
        return Err(refuse(path, "it is a symbolic link"));
    }
    if !kind.is_dir() {
        return Err(refuse(path, "it is not a directory"));
    }
    let entries = list(path)?;
    if entries.is_empty() {
        return Ok(());
    }
    let previous = previous_export_files(path, &entries)?;
    for name in previous {
        let file = path.join(&name);
        std::fs::remove_file(&file).map_err(|e| OpsError::io(&file, e))?;
    }
    Ok(())
}

fn list(path: &Path) -> Result<Vec<String>> {
    std::fs::read_dir(path)
        .map_err(|e| OpsError::io(path, e))?
        .map(|entry| {
            entry
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .map_err(|e| OpsError::io(path, e))
        })
        .collect()
}

/// The directory's files, but only if it is exactly the output of an earlier export: a regular
/// `manifest.json` that parses, and nothing but regular files it lists. Anything else is refused.
fn previous_export_files(dir: &Path, entries: &[String]) -> Result<Vec<String>> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let is_regular = |p: &Path| {
        std::fs::symlink_metadata(p)
            .map(|m| m.file_type().is_file())
            .unwrap_or(false)
    };
    if !is_regular(&manifest_path) {
        return Err(refuse(dir, "it is not empty and holds no earlier export"));
    }
    let text =
        std::fs::read_to_string(&manifest_path).map_err(|e| OpsError::io(&manifest_path, e))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|_| refuse(dir, "manifest.json is not a PAIR export manifest"))?;
    let known: Vec<&str> = manifest
        .files
        .iter()
        .map(|f| f.name.as_str())
        .chain([MANIFEST_FILE])
        .collect();
    for name in entries {
        if !known.contains(&name.as_str()) || !is_regular(&dir.join(name)) {
            return Err(refuse(
                dir,
                &format!("{name:?} is not part of an earlier export (or is not a plain file)"),
            ));
        }
    }
    Ok(entries.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn scratch() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pair_ops_out_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// The race after `prepare`: something appears at a file name before it is written. Writing
    /// must fail (`O_EXCL`) and must never follow a symlink to its target.
    #[tokio::test]
    async fn test_write_refuses_existing_path_and_never_follows_symlink() {
        let root = scratch();
        let victim = root.join("victim");
        std::fs::write(&victim, "keep").expect("victim");
        let out = root.join("out");
        let dir = OutDir::prepare(&out).await.expect("prepare");
        symlink(&victim, out.join("planted.jsonl")).expect("plant symlink");
        std::fs::write(out.join("existing.jsonl"), "old").expect("existing file");

        assert!(dir.write("planted.jsonl", "new").await.is_err());
        assert!(dir.write("existing.jsonl", "new").await.is_err());

        assert_eq!(std::fs::read_to_string(&victim).expect("victim"), "keep");
        assert_eq!(
            std::fs::read_to_string(out.join("existing.jsonl")).expect("file"),
            "old"
        );
        dir.write("fresh.jsonl", "x")
            .await
            .expect("a new name is fine");
    }
}
