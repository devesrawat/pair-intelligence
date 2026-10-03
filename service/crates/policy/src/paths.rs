use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const DOCKER_SOCKET_NAME: &str = "docker.sock";

// One generic reason per category. Reasons are returned to the caller, so they never contain a
// resolved path, a symlink target or an OS error (that would be a filesystem probe).
const REASON_INVALID: &str = "invalid path";
const REASON_CREDENTIAL: &str = "host credential path denied";
const REASON_OUTSIDE: &str = "path is outside the workspace";
const REASON_UNRESOLVABLE: &str = "path cannot be resolved safely";

/// Resolves paths against the real filesystem and rejects host credentials and workspace escapes.
#[derive(Debug, Clone)]
pub struct PathGuard {
    home: PathBuf,
    denied: Vec<PathBuf>,
    denied_names: Vec<String>,
    allowed_names: Vec<String>,
}

impl PathGuard {
    pub fn new(home: &Path, denied_paths: &[String], denied_names: &[String]) -> Self {
        let mut denied = Vec::with_capacity(denied_paths.len() * 2);
        for raw in denied_paths {
            let expanded = expand_home(home, raw);
            if let Ok(canonical) = expanded.canonicalize() {
                denied.push(canonical);
            }
            denied.push(expanded);
        }
        Self {
            home: home.to_path_buf(),
            denied,
            denied_names: denied_names.iter().map(|n| n.to_lowercase()).collect(),
            allowed_names: Vec::new(),
        }
    }

    /// Returns the fully resolved path, or the reason it is refused. `workspace` must be canonical.
    pub fn check(&self, workspace: &Path, raw: &str) -> Result<PathBuf, String> {
        self.check_in(workspace, workspace, raw)
    }

    /// Like [`Self::check`]; `declared_root` is the workspace as configured (possibly through a
    /// symlink such as macOS `/var` -> `/private/var`), so an absolute path spelled through that
    /// alias is recognised without the filesystem.
    ///
    /// Order: lexical credential check, lexical workspace check (an absolute path outside the
    /// workspace is refused on its text alone, before any filesystem access), then resolution.
    pub fn check_in(
        &self,
        workspace: &Path,
        declared_root: &Path,
        raw: &str,
    ) -> Result<PathBuf, String> {
        if raw.is_empty() || raw.contains('\0') {
            return Err(REASON_INVALID.to_owned());
        }
        let candidate = if raw == "~" || raw.starts_with("~/") {
            expand_home(&self.home, raw)
        } else {
            workspace.join(raw)
        };
        self.refuse_credentials(&candidate)?;
        let lexical = lexically_normalized(&candidate);
        if !lexical.starts_with(workspace) && !lexical.starts_with(declared_root) {
            return Err(REASON_OUTSIDE.to_owned());
        }
        let resolved = resolve(&candidate)?;
        self.refuse_credentials(&resolved)?;
        if resolved.starts_with(workspace) {
            Ok(resolved)
        } else {
            Err(REASON_OUTSIDE.to_owned())
        }
    }

    /// Exact file names that are never treated as credentials (templates like `.env.example`).
    #[must_use]
    pub fn with_allowed_names(mut self, names: &[String]) -> Self {
        self.allowed_names = names.iter().map(|n| n.to_lowercase()).collect();
        self
    }

    fn refuse_credentials(&self, path: &Path) -> Result<(), String> {
        let is_socket = path.file_name().is_some_and(|n| n == DOCKER_SOCKET_NAME);
        let named = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .is_some_and(|n| {
                !self.allowed_names.contains(&n)
                    && self.denied_names.iter().any(|d| name_matches(d, &n))
            });
        if is_socket || named || self.denied.iter().any(|d| path.starts_with(d)) {
            return Err(REASON_CREDENTIAL.to_owned());
        }
        Ok(())
    }
}

/// `*.ext` matches by suffix, `prefix*` by prefix, anything else exactly. Both sides are lowercase.
fn name_matches(pattern: &str, name: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        name.ends_with(suffix)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        pattern == name
    }
}

fn expand_home(home: &Path, raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

/// Collapses `.` and `..` without touching the filesystem. A `..` at the root stays at the root.
fn lexically_normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Canonicalizes the deepest existing ancestor and re-attaches the not-yet-existing tail.
/// `..` in the tail and dangling symlinks are refused because their target is unknowable.
/// Failures are logged with detail but returned as one generic reason.
fn resolve(path: &Path) -> Result<PathBuf, String> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match current.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(tail.iter().rev());
                return Ok(resolved);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if current.symlink_metadata().is_ok() {
                    tracing::debug!(path = %current.display(), "dangling symlink refused");
                    return Err(REASON_UNRESOLVABLE.to_owned());
                }
                let name = match current.components().next_back() {
                    Some(Component::Normal(n)) => n.to_os_string(),
                    _ => return Err(unresolvable(path, "no final component")),
                };
                tail.push(name);
                if !current.pop() {
                    return Err(unresolvable(path, "no parent"));
                }
            }
            Err(e) => return Err(unresolvable(path, &e.to_string())),
        }
    }
}

fn unresolvable(path: &Path, detail: &str) -> String {
    tracing::debug!(path = %path.display(), detail, "path could not be resolved");
    REASON_UNRESOLVABLE.to_owned()
}
