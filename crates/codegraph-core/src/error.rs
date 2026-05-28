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
}

pub type Result<T> = std::result::Result<T, CoreError>;
