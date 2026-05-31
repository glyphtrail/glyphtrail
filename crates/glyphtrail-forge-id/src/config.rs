//! Forge token configuration: a host → token-env / forge-kind mapping.
//!
//! The [`crate::api`] numeric-id resolver recognises the public forges by host
//! (github.com, gitlab.com, codeberg.org) and reads well-known token env vars.
//! This config layers on top: it maps an arbitrary host to the env var holding
//! its token (so a token under a non-standard name is usable without aliasing)
//! and — for self-hosted instances the tool can't recognise by host — declares
//! the forge `kind`.
//!
//! ```toml
//! [hosts."codeberg.org"]
//! token_env = "CODEBERG_READ_ONLY_PAT"
//!
//! [hosts."git.example.com"]
//! kind = "gitea"            # github | gitlab | gitea
//! token_env = "EXAMPLE_TOKEN"
//! ```
//!
//! This only loads the mapping from a TOML file the caller chooses; resolving
//! env vars and making the API call is the resolver's job.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::ForgeIdError;

/// A recognised forge API flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    GitHub,
    GitLab,
    Gitea,
}

/// Per-host configuration: which forge API it speaks (for hosts not recognised
/// by name) and the env var holding its token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeHost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ForgeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
}

/// Host → forge configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeConfig {
    #[serde(default)]
    pub hosts: HashMap<String, ForgeHost>,
}

impl ForgeConfig {
    /// Load the config from `path`, returning an empty config when the file is
    /// absent. A malformed file is an error. The host application decides where
    /// the file lives and may treat any error as empty (a best-effort load).
    pub fn load(path: &Path) -> Result<ForgeConfig, ForgeIdError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ForgeIdError::Parse {
                path: path.to_path_buf(),
                source: Box::new(source),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ForgeConfig::default()),
            Err(source) => Err(ForgeIdError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Load from `path`, treating a missing/garbled file as an empty config —
    /// the resolver then falls back to built-in host recognition + well-known
    /// env vars.
    pub fn load_or_default(path: &Path) -> ForgeConfig {
        ForgeConfig::load(path).unwrap_or_default()
    }

    /// Configuration for `host`, if any.
    pub fn for_host(&self, host: &str) -> Option<&ForgeHost> {
        self.hosts.get(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn parses_host_mapping() {
        let text = r#"
            [hosts."codeberg.org"]
            token_env = "CODEBERG_READ_ONLY_PAT"

            [hosts."git.example.com"]
            kind = "gitea"
            token_env = "EXAMPLE_TOKEN"
        "#;
        let cfg: ForgeConfig = toml::from_str(text).unwrap();
        check!(
            cfg.for_host("codeberg.org").unwrap().token_env.as_deref()
                == Some("CODEBERG_READ_ONLY_PAT")
        );
        check!(cfg.for_host("codeberg.org").unwrap().kind == None);
        let acme = cfg.for_host("git.example.com").unwrap();
        check!(acme.kind == Some(ForgeKind::Gitea));
        check!(acme.token_env.as_deref() == Some("EXAMPLE_TOKEN"));
        check!(cfg.for_host("unknown.org") == None);
    }

    #[test]
    fn missing_file_is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("glyphtrail-forge-{nanos}.toml"));
        check!(ForgeConfig::load(&path).unwrap() == ForgeConfig::default());
    }
}
