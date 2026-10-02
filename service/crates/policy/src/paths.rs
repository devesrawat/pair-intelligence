use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const DOCKER_SOCKET_NAME: &str = "docker.sock";

/// Resolves paths against the real filesystem and rejects host credentials and workspace escapes.
#[derive(Debug, Clone)]
pub struct PathGuard {
    home: PathBuf,
    denied: Vec<PathBuf>,
}

impl PathGuard {
    pub fn new(home: &Path, denied_paths: &[String]) -> Self {
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

    fn refuse_credentials(&self, path: &Path) -> Result<(), String> {
        let is_socket = path.file_name().is_some_and(|n| n == DOCKER_SOCKET_NAME);
        if is_socket || self.denied.iter().any(|d| path.starts_with(d)) {
            return Err(format!("host credential path denied: {}", path.display()));
        }
        Ok(())
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
