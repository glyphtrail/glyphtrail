use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::CoreError;
use crate::api::Protocol;
use crate::rewrite::PrefixRewrite;

/// Per-repo layout. The index lives in `<repo>/.meridian/`.
pub const INDEX_DIR: &str = ".meridian";
pub const DB_FILE: &str = "graph.db";
pub const IGNORE_FILE: &str = ".meridianignore";
/// Optional per-repo config file, read from `<repo>/.meridian/config.toml`.
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

/// Per-repo configuration, read from `.meridian/config.toml`. Every field has a
/// default, so a missing or partial file is always valid.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub api: ApiConfig,
    /// Extra languages to load at runtime (the dynamic half of hybrid language
    /// support). Each names a grammar + query the analyzer compiles on demand.
    #[serde(rename = "languages")]
    pub languages: Vec<DynamicLanguage>,
    /// Extra directory names to skip during discovery, on top of the built-in
    /// defaults (build/output/VCS dirs). A bare name (`vendor`) matches at any
    /// depth; a gitignore-style glob (`gitnexus/vendor/**/build`) is honored too.
    pub ignore_dirs: Vec<String>,
    /// Impact-analysis tuning.
    pub impact: ImpactConfig,
    /// Security / sensitive-data handling.
    pub security: SecurityConfig,
}

/// Security configuration.
///
/// ```toml
/// [security]
/// record_sensitive_files = true
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// Credential/key files are always excluded from parsing. With this set, a
    /// content-less record (a `File` node, no bytes read) is still added so the
    /// graph shows the file *exists* without exposing its values. Default off:
    /// secrets leave no trace at all.
    pub record_sensitive_files: bool,
}

/// Impact-analysis configuration.
///
/// ```toml
/// [impact]
/// test_globs = ["it/**", "*_it.rs"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImpactConfig {
    /// Extra glob patterns (repo-relative, gitignore-style) whose matching files
    /// classify as tests, on top of the built-in heuristics. Built-in detection
    /// is unchanged when this is empty.
    pub test_globs: Vec<String>,
}

/// A language identified by file extension but not built in: its tree-sitter
/// grammar is compiled from `grammar` at runtime and paired with `query`.
///
/// ```toml
/// [[languages]]
/// name = "ruby"
/// extensions = ["rb"]
/// grammar = "grammars/tree-sitter-ruby/src"  # dir with parser.c + grammar.json
/// query = "queries/ruby.scm"                 # @def.<kind>/@call/@import/...
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicLanguage {
    pub name: String,
    pub extensions: Vec<String>,
    /// Path (relative to the repo root) to the grammar's tree-sitter `src` dir.
    pub grammar: PathBuf,
    /// Path (relative to the repo root) to the `.scm` extraction query.
    pub query: PathBuf,
}

impl Config {
    /// Load config for a repo, returning defaults when the file is absent.
    pub fn load(repo_root: impl AsRef<Path>) -> crate::Result<Config> {
        let path = RepoPaths::new(repo_root).config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(CoreError::ConfigRead { path, source }),
        }
    }

    pub fn from_toml_str(text: &str) -> crate::Result<Config> {
        Config::parse(text, Path::new("config.toml"))
    }

    /// Parse and validate config, attaching `path` to any error for diagnostics.
    fn parse(text: &str, path: &Path) -> crate::Result<Config> {
        let cfg: Config = toml::from_str(text).map_err(|source| CoreError::ConfigParse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    fn validate(&self, path: &Path) -> crate::Result<()> {
        let invalid = |message: String| CoreError::ConfigInvalid {
            path: path.to_path_buf(),
            message,
        };
        for r in &self.api.rewrites {
            r.validate().map_err(&invalid)?;
        }
        for p in &self.api.gateway_prefixes {
            crate::rewrite::validate_gateway_prefix(p).map_err(&invalid)?;
        }
        Ok(())
    }
}

/// Cross-boundary API-linking configuration.
///
/// Confidence: explicit [`ApiConfig::rewrites`] describe the real gateway
/// mapping and produce high-confidence (`Extracted`) matches; the
/// `heuristic_prefix_strip` fallback contributes low-confidence (`Inferred`)
/// candidates. The two are additive — the heuristic still runs alongside
/// explicit rewrites, and when both yield the same path the `Extracted`
/// confidence wins.
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
    /// Additionally strip well-known gateway prefixes as a low-confidence
    /// fallback. Runs independently of (alongside) explicit rewrites.
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

/// How to parse a blessed schema artifact. `Auto` (default) dispatches by
/// `protocol`; `Hasura` parses Hasura metadata (tables + RESTified endpoints).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaFormat {
    #[default]
    Auto,
    Hasura,
}

/// A local API schema file blessed for reconciliation against extracted endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaSource {
    pub path: String,
    /// Validated at parse time against the known protocols (rest/grpc/graphql).
    /// Ignored when `format = "hasura"`.
    #[serde(default = "default_protocol")]
    pub protocol: Protocol,
    /// Parser to use; defaults to dispatching by `protocol`.
    #[serde(default)]
    pub format: SchemaFormat,
}

fn default_protocol() -> Protocol {
    Protocol::Rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = Config::from_toml_str("").unwrap();
        check!(cfg.api.heuristic_prefix_strip);
        check!(cfg.api.gateway_prefixes == vec!["/api".to_string()]);
        check!(cfg.api.rewrites.is_empty());
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
        check!(!cfg.api.heuristic_prefix_strip);
        check!(cfg.api.gateway_prefixes.len() == 2);
        check!(cfg.api.rewrites.len() == 1);
        check!(cfg.api.rewrites[0].from == "/api");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        check!(Config::from_toml_str("[api]\nbogus = 1\n").is_err());
    }

    #[test]
    fn root_prefix_rewrite_is_rejected() {
        let err = Config::from_toml_str(
            r#"
            [[api.rewrites]]
            from = "/"
            to = "/x"
            "#,
        )
        .unwrap_err();
        check!(matches!(err, crate::CoreError::ConfigInvalid { .. }));
    }

    #[test]
    fn invalid_gateway_prefix_is_rejected() {
        for bad in ["\"/\"", "\"\"", "\"   \""] {
            let toml = format!("[api]\ngateway_prefixes = [{bad}]\n");
            let err = Config::from_toml_str(&toml).unwrap_err();
            check!(
                matches!(err, crate::CoreError::ConfigInvalid { .. }),
                "expected ConfigInvalid for {bad}, got {err:?}"
            );
        }
        // A real prefix is accepted.
        check!(Config::from_toml_str("[api]\ngateway_prefixes = [\"/api\"]\n").is_ok());
    }

    #[test]
    fn parses_dynamic_languages() {
        let cfg = Config::from_toml_str(
            r#"
            [[languages]]
            name = "ruby"
            extensions = ["rb", "rake"]
            grammar = "grammars/tree-sitter-ruby/src"
            query = "queries/ruby.scm"
            "#,
        )
        .unwrap();
        check!(cfg.languages.len() == 1);
        let lang = &cfg.languages[0];
        check!(lang.name == "ruby");
        check!(lang.extensions == ["rb", "rake"]);
        check!(lang.grammar == std::path::Path::new("grammars/tree-sitter-ruby/src"));
        check!(lang.query == std::path::Path::new("queries/ruby.scm"));
    }

    #[test]
    fn parses_security_record_sensitive_files() {
        let cfg = Config::from_toml_str("[security]\nrecord_sensitive_files = true\n").unwrap();
        check!(cfg.security.record_sensitive_files);
        // Off by default.
        check!(
            !Config::from_toml_str("")
                .unwrap()
                .security
                .record_sensitive_files
        );
    }

    #[test]
    fn parses_impact_test_globs() {
        let cfg =
            Config::from_toml_str("[impact]\ntest_globs = [\"it/**\", \"*_it.rs\"]\n").unwrap();
        check!(cfg.impact.test_globs == ["it/**", "*_it.rs"]);
        // Absent by default.
        check!(
            Config::from_toml_str("")
                .unwrap()
                .impact
                .test_globs
                .is_empty()
        );
    }

    #[test]
    fn parses_extra_ignore_dirs() {
        let cfg = Config::from_toml_str("ignore_dirs = [\"vendor\", \"gen/**/out\"]\n").unwrap();
        check!(cfg.ignore_dirs == ["vendor", "gen/**/out"]);
        // Absent by default.
        check!(Config::from_toml_str("").unwrap().ignore_dirs.is_empty());
    }

    #[test]
    fn schema_protocol_is_validated() {
        let cfg = Config::from_toml_str(
            r#"
            [[api.schemas]]
            path = "openapi.json"
            protocol = "rest"
            "#,
        )
        .unwrap();
        check!(cfg.api.schemas[0].protocol == Protocol::Rest);
        // An unknown protocol fails at parse time.
        let err = Config::from_toml_str(
            r#"
            [[api.schemas]]
            path = "x.proto"
            protocol = "soap"
            "#,
        )
        .unwrap_err();
        check!(matches!(err, crate::CoreError::ConfigParse { .. }));
    }
}
