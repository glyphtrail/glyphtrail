use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::rewrite::PrefixRewrite;

/// Per-repo layout. The index lives in `<repo>/.codegraph/`.
pub const INDEX_DIR: &str = ".codegraph";
pub const DB_FILE: &str = "graph.db";
pub const IGNORE_FILE: &str = ".codegraphignore";
/// Optional per-repo config file, read from `<repo>/.codegraph/config.toml`.
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub root: PathBuf,
    pub index_dir: PathBuf,
    pub db_path: PathBuf,
}

impl RepoPaths {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let index_dir = root.join(INDEX_DIR);
        let db_path = index_dir.join(DB_FILE);
        Self {
            root,
            index_dir,
            db_path,
        }
    }

    pub fn ensure_index_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.index_dir)
    }

    /// Path to the optional per-repo config file.
    pub fn config_path(&self) -> PathBuf {
        self.index_dir.join(CONFIG_FILE)
    }
}

/// Per-repo configuration, read from `.codegraph/config.toml`. Every field has a
/// default, so a missing or partial file is always valid.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub api: ApiConfig,
}

impl Config {
    /// Load config for a repo, returning defaults when the file is absent.
    pub fn load(repo_root: impl AsRef<Path>) -> crate::Result<Config> {
        let path = RepoPaths::new(repo_root).config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::from_toml_str(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(crate::CoreError::Io(e)),
        }
    }

    pub fn from_toml_str(text: &str) -> crate::Result<Config> {
        toml::from_str(text).map_err(|e| crate::CoreError::Config(e.to_string()))
    }
}

/// Cross-boundary API-linking configuration.
///
/// Precedence: explicit [`ApiConfig::rewrites`] describe the real gateway
/// mapping and produce high-confidence (`Extracted`) matches; the
/// `heuristic_prefix_strip` fallback produces low-confidence (`Inferred`) ones.
///
/// ```toml
/// [api]
/// heuristic_prefix_strip = true
/// gateway_prefixes = ["/api"]
///
/// # Envoy strips the /api prefix the service mounts under:
/// [[api.rewrites]]
/// from = "/api"
/// to = ""
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// Strip well-known gateway prefixes when no explicit rewrite matches.
    pub heuristic_prefix_strip: bool,
    /// Prefixes the heuristic may strip (segment-boundary matched).
    pub gateway_prefixes: Vec<String>,
    /// Explicit, authoritative path rewrites (applied internal → external).
    pub rewrites: Vec<PrefixRewrite>,
    /// Blessed schema artifacts to reconcile against (consumed by schema ingestion).
    pub schemas: Vec<SchemaSource>,
    /// Framework hints to steer extraction (consumed by the extractors).
    pub frameworks: Vec<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            heuristic_prefix_strip: true,
            gateway_prefixes: vec!["/api".to_string()],
            rewrites: Vec::new(),
            schemas: Vec::new(),
            frameworks: Vec::new(),
        }
    }
}

/// A local API schema file blessed for reconciliation against extracted endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaSource {
    pub path: String,
    pub protocol: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = Config::from_toml_str("").unwrap();
        assert!(cfg.api.heuristic_prefix_strip);
        assert_eq!(cfg.api.gateway_prefixes, vec!["/api".to_string()]);
        assert!(cfg.api.rewrites.is_empty());
    }

    #[test]
    fn parses_rewrites_and_prefixes() {
        let cfg = Config::from_toml_str(
            r#"
            [api]
            heuristic_prefix_strip = false
            gateway_prefixes = ["/api", "/internal"]

            [[api.rewrites]]
            from = "/api"
            to = ""
            "#,
        )
        .unwrap();
        assert!(!cfg.api.heuristic_prefix_strip);
        assert_eq!(cfg.api.gateway_prefixes.len(), 2);
        assert_eq!(cfg.api.rewrites.len(), 1);
        assert_eq!(cfg.api.rewrites[0].from, "/api");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(Config::from_toml_str("[api]\nbogus = 1\n").is_err());
    }
}
