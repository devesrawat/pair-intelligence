use thiserror::Error;

pub type Result<T> = std::result::Result<T, OpsError>;

#[derive(Debug, Error)]
pub enum OpsError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// A purge target every deployment must have could not run (missing table or column, or a
    /// different table shadowing it on the search_path). Nothing was erased.
    #[error("retention purge refused: core target(s) cannot run in this schema: {}", .0.join(", "))]
    CoreTargetSkipped(Vec<String>),
}

impl OpsError {
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}
