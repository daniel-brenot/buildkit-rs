//! Errors produced while pulling images or executing a build.

use std::path::PathBuf;

/// Errors produced while pulling images or executing a build.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `FROM` / tag string could not be parsed as an OCI reference.
    #[error("invalid image reference: {0}")]
    Reference(String),
    /// Registry HTTP or protocol failure while pulling.
    #[error("registry error: {0}")]
    Registry(#[from] oci_distribution::errors::OciDistributionError),
    /// Catch-all for build, unpack, and validation failures.
    #[error("{0}")]
    Other(String),
    /// Filesystem error, with the path that was being accessed when known.
    #[error("failed to read {path}: {source}")]
    Io {
        /// Path related to the failure (`<io>` when the origin is unknown).
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Image `config.json` could not be parsed.
    #[error("invalid image config: {0}")]
    Config(#[from] serde_json::Error),
    /// Dockerfile failed to parse.
    #[error("{0}")]
    Dockerfile(#[from] dockerfile::Error),
    /// The [`crate::Backend`] returned `Err` from [`crate::Backend::run`].
    #[error("backend: {0}")]
    Backend(String),
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io {
            path: PathBuf::from("<io>"),
            source,
        }
    }
}

impl Error {
    pub(crate) fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    pub(crate) fn backend(err: impl std::fmt::Display) -> Self {
        Error::Backend(err.to_string())
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    /// Whether this is an I/O error for a missing path.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound
        )
    }

    /// Raw OS error code when this is an I/O failure.
    pub fn raw_os_error(&self) -> Option<i32> {
        match self {
            Error::Io { source, .. } => source.raw_os_error(),
            _ => None,
        }
    }
}

#[allow(dead_code)]
pub(crate) type Result<T> = std::result::Result<T, Error>;
