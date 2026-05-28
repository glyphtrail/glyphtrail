use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
