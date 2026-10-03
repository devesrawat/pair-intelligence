//! Structured edit sets from the model, applied only if the whole set is in scope.
use super::scope::{normalize_rel, Scope};
use crate::{limits::RunLimits, tools::FS_WRITE};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{TaskId, TraceId},
    types::{ActionRequest, DataClass, PolicyContext},
};
use pair_policy::Gate;
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
    let set: EditSet = serde_json::from_str(&text[start..=end])
        .map_err(|e| invalid(format!("edit output invalid: {e}")))?;
    Ok(set.edits)
}

fn reject_symlinks(root: &Path, rel: &str) -> Result<()> {
    let mut cur = root.to_path_buf();
    for seg in rel.split('/') {
        cur.push(seg);
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return Err(PairError::new(
                    ErrorCode::PolicyDenied,
                    format!("symlink in edit path: {rel}"),
                ));
            }
        }
    }
    Ok(())
}

/// Who is writing: every write goes through `Gate::execute` as one `fs.write` request.
pub struct EditContext<'a> {
    pub gate: &'a Gate,
    pub policy: &'a PolicyContext,
    pub task: TaskId,
    pub trace: TraceId,
    pub data_class: DataClass,
    pub limits: &'a RunLimits,
}

/// Validates every edit first (scope, traversal, symlinks, size), then writes the whole set
/// inside ONE `Gate::execute` call for the registered `fs.write` tool listing every target
/// path, so policy (workspace confinement, `denied_paths`) sees exactly what will be touched
/// and a denial leaves every file untouched. Counts as one tool call.
/// Returns the normalised paths written.
pub async fn apply_edits(
    cx: &EditContext<'_>,
    root: &Path,
    scope: &Scope,
    edits: &[FileEdit],
) -> Result<Vec<String>> {
    let mut planned = Vec::with_capacity(edits.len());
    for e in edits {
        let rel = normalize_rel(&e.path)?;
        if e.content.len() > MAX_EDIT_BYTES {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                format!("edit too large: {rel}"),
            ));
        }
        scope.check_all([rel.as_str()])?;
        reject_symlinks(root, &rel)?;
        planned.push((rel, &e.content));
    }
    let req = ActionRequest {
        tool: FS_WRITE.to_string(),
        executable: None,
        args: Vec::new(),
        paths: planned
            .iter()
            .map(|(rel, _)| root.join(rel).display().to_string())
            .collect(),
        destination: None,
        data_class: cx.data_class,
        task: cx.task,
        trace: cx.trace,
    };
    cx.limits.begin_tool_call()?;
    cx.gate
        .execute(&req, cx.policy, || async { write_all(root, &planned) })
        .await?;
    Ok(planned.into_iter().map(|(r, _)| r).collect())
}

fn write_all(root: &Path, planned: &[(String, &String)]) -> Result<()> {
    let io = |rel: &str, e: std::io::Error| {
        PairError::new(ErrorCode::Internal, format!("write {rel}: {e}"))
    };
    for (rel, content) in planned {
        let target = root.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(rel, e))?;
        }
        std::fs::write(&target, content).map_err(|e| io(rel, e))?;
    }
    Ok(())
}
