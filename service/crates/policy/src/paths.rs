use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const DOCKER_SOCKET_NAME: &str = "docker.sock";

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
        if raw.is_empty() || raw.contains('\0') {
            return Err(format!("invalid path {raw:?}"));
        }
        let candidate = if raw == "~" || raw.starts_with("~/") {
            expand_home(&self.home, raw)
        } else {
            workspace.join(raw)
        };
        self.refuse_credentials(&candidate)?;
        let resolved = resolve(&candidate)?;
        self.refuse_credentials(&resolved)?;
        if resolved.starts_with(workspace) {
            Ok(resolved)
        } else {
            Err(format!("path {raw:?} resolves outside the workspace"))
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
            return Err(format!("host credential path denied: {}", path.display()));
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

/// Canonicalizes the deepest existing ancestor and re-attaches the not-yet-existing tail.
/// `..` in the tail and dangling symlinks are refused because their target is unknowable.
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
                    return Err(format!("dangling symlink at {}", current.display()));
                }
                let name = match current.components().next_back() {
                    Some(Component::Normal(n)) => n.to_os_string(),
                    _ => return Err(format!("unresolvable path {}", path.display())),
                };
                tail.push(name);
                if !current.pop() {
                    return Err(format!("unresolvable path {}", path.display()));
                }
            }
            Err(e) => return Err(format!("cannot resolve {}: {e}", path.display())),
        }
    }
}
