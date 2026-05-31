//! Error type for forge-id config loading.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ForgeIdError {
    #[error("failed to read forge config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid forge config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
}
