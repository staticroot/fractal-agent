use std::path::PathBuf;

/// One error type for every authority-free mechanism in this crate. Callers in
/// the agent map these onto the socket protocol; the crate itself never decides
/// policy, so the variants describe *what broke*, not *who was allowed*.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("git: {0}")]
    Git(String),

    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("nix: {0}")]
    Nix(String),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// A staged value would collide with the shape of the model: it would either
    /// swallow an existing subtree or sit beneath a key already holding a leaf.
    /// Replacing silently in either case is the quiet corruption the tree model
    /// exists to prevent.
    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
