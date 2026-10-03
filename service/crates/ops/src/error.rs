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
}

impl OpsError {
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}
