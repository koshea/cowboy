//! Error types for cowboy-core.

use std::path::PathBuf;

/// Result alias used throughout cowboy-core.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced while loading or validating configuration and other
/// core operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config file not found: {0}")]
    ConfigNotFound(PathBuf),

    #[error("failed to read {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },

    /// A security invariant was violated by the configuration. These must
    /// never be silently ignored — they protect the host boundary.
    #[error("security invariant violated: {0}")]
    SecurityInvariant(String),

    #[error("invalid configuration: {0}")]
    Invalid(String),

    #[error("model error: {0}")]
    Model(String),

    /// The provider says this model does not exist (a 404 `model_not_found`, or a
    /// 400 naming an unknown model). Distinct from [`Self::Model`] because it is
    /// **permanent** — retrying is pointless, but *rerouting* to another model is
    /// not, so the agent loop can fall back instead of failing the session.
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
