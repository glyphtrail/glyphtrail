use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid config at {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("invalid config at {path}: {message}")]
    ConfigInvalid { path: PathBuf, message: String },
    #[error("failed to read config {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid repo registry at {path}: {source}")]
    RegistryParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid Cargo manifest: {source}")]
    ManifestParse {
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("could not lock {path}: {message}")]
    Lock { path: PathBuf, message: String },
}

pub type Result<T> = std::result::Result<T, CoreError>;
