//! Structured edit sets from the model, applied only if the whole set is in scope.
use super::scope::{normalize_rel, Scope};
use pair_core::error::{ErrorCode, PairError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_EDIT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
struct EditSet {
    edits: Vec<FileEdit>,
}

/// Parses `{"edits":[{"path","content"}]}`, tolerating a surrounding code fence.
pub fn parse_edit_set(text: &str) -> Result<Vec<FileEdit>> {
    let invalid = |m: String| PairError::new(ErrorCode::InvalidInput, m);
    let (start, end) = match (text.find('{'), text.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => return Err(invalid("edit output contains no JSON object".into())),
    };
    let set: EditSet = serde_json::from_str(&text[start..=end]).map_err(|e| invalid(format!("edit output invalid: {e}")))?;
    Ok(set.edits)
}

fn reject_symlinks(root: &Path, rel: &str) -> Result<()> {
    let mut cur = root.to_path_buf();
    for seg in rel.split('/') {
        cur.push(seg);
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return Err(PairError::new(ErrorCode::PolicyDenied, format!("symlink in edit path: {rel}")));
            }
        }
    }
    Ok(())
}

/// Validates every edit first (scope, traversal, symlinks, size); writes nothing if any fails.
/// Returns the normalised paths written.
pub fn apply_edits(root: &Path, scope: &Scope, edits: &[FileEdit]) -> Result<Vec<String>> {
    let mut planned = Vec::with_capacity(edits.len());
    for e in edits {
        let rel = normalize_rel(&e.path)?;
        if e.content.len() > MAX_EDIT_BYTES {
            return Err(PairError::new(ErrorCode::InvalidInput, format!("edit too large: {rel}")));
        }
        scope.check_all([rel.as_str()])?;
        reject_symlinks(root, &rel)?;
        planned.push((rel, &e.content));
    }
    let io = |rel: &str, e: std::io::Error| PairError::new(ErrorCode::Internal, format!("write {rel}: {e}"));
    for (rel, content) in &planned {
        let target = root.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(rel, e))?;
        }
        std::fs::write(&target, content).map_err(|e| io(rel, e))?;
    }
    Ok(planned.into_iter().map(|(r, _)| r).collect())
}
