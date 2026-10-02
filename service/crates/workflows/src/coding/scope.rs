//! Patch scope: only declared paths may change; credential-looking files never.
use pair_core::error::{ErrorCode, PairError, Result};
use serde::{Deserialize, Serialize};

const SECRET_FILE_NAMES: [&str; 6] = [
    ".netrc",
    "credentials",
    "id_rsa",
    "id_ed25519",
    ".npmrc",
    ".pypirc",
];
const SECRET_EXTENSIONS: [&str; 5] = ["pem", "key", "p12", "pfx", "keystore"];

/// Allowed paths: an entry ending in `/` is a directory prefix, anything else an exact file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub allow: Vec<String>,
}

/// True when `path` (repo-relative) looks like a credential file.
pub fn is_secret_path(path: &str) -> bool {
    path.split('/').any(|seg| {
        let lower = seg.to_ascii_lowercase();
        lower == ".env"
            || lower.starts_with(".env.")
            || SECRET_FILE_NAMES
                .iter()
                .any(|n| lower == *n || lower.starts_with(&format!("{n}.")))
            || lower
                .rsplit_once('.')
                .is_some_and(|(_, ext)| SECRET_EXTENSIONS.contains(&ext))
    })
}

/// Rejects absolute paths, `..` components and `.git`; returns the normalised path.
pub fn normalize_rel(path: &str) -> Result<String> {
    let trimmed = path.trim();
    let bad = trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.split('/').any(|s| s == ".." || s == ".git");
    if bad {
        return Err(PairError::new(
            ErrorCode::PolicyDenied,
            format!("path escapes workspace: {path:?}"),
        ));
    }
    let parts: Vec<&str> = trimmed
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.is_empty() {
        return Err(PairError::new(
            ErrorCode::PolicyDenied,
            format!("empty path: {path:?}"),
        ));
    }
    Ok(parts.join("/"))
}

impl Scope {
    pub fn new(allow: Vec<String>) -> Self {
        Self { allow }
    }

    pub fn permits(&self, path: &str) -> bool {
        let Ok(norm) = normalize_rel(path) else {
            return false;
        };
        if is_secret_path(&norm) {
            return false;
        }
        self.allow.iter().any(|a| {
            if a.ends_with('/') {
                norm.starts_with(a.as_str())
            } else {
                norm == *a
            }
        })
    }

    /// Whole-patch check: one out-of-scope path rejects everything.
    pub fn check_all<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let outside: Vec<&str> = paths.into_iter().filter(|p| !self.permits(p)).collect();
        if outside.is_empty() {
            Ok(())
        } else {
            Err(PairError::new(
                ErrorCode::PolicyDenied,
                format!(
                    "patch touches paths outside declared scope: {}",
                    outside.join(", ")
                ),
            ))
        }
    }
}
