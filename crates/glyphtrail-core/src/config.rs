use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::CoreError;
use crate::api::Protocol;
use crate::link_hints::{HINTS_FILE, LOCAL_HINTS_FILE, LinkHint};
use crate::rewrite::PrefixRewrite;

/// Per-repo layout. The index lives in `<repo>/.glyphtrail/`.
pub const INDEX_DIR: &str = ".glyphtrail";
pub const DB_FILE: &str = "graph.db";
pub const IGNORE_FILE: &str = ".glyphtrailignore";
/// The unified per-repo config file. Committed at the repo root
/// (`<repo>/glyphtrail.toml`, team-shared); an optional same-named file inside
/// `.glyphtrail/` is a gitignored personal override merged on top. Holds the
/// analysis settings *and* the cross-repo `[[links]]` hints.
pub const CONFIG_FILE: &str = "glyphtrail.toml";
/// Pre-unification config file (analysis only), still read for back-compat from
/// `<repo>/.glyphtrail/config.toml` and folded into the unified file on the next
/// `config`/`link` edit.
pub const LEGACY_CONFIG_FILE: &str = "config.toml";

/// Pre-rename names, silently migrated to the current ones when found (#293,
/// the project was `stratograph`). The per-repo ignore file is still *honored*
/// under its old name (see the analyze walker); these dirs/files are moved.
pub const LEGACY_INDEX_DIR: &str = ".stratograph";
pub const LEGACY_IGNORE_FILE: &str = ".stratographignore";

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
        // Silently upgrade a pre-rename index dir (#293): rename `.stratograph`
        // to `.glyphtrail` when the old one exists and the new one doesn't, so an
        // index built before the rename keeps working without a reanalyze. Best
        // effort — a failure just means it gets rebuilt under the new name.
        let legacy = root.join(LEGACY_INDEX_DIR);
        if legacy.is_dir() && !index_dir.exists() {
            let _ = std::fs::rename(&legacy, &index_dir);
        }
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

    /// The committed, team-shared config file at the repo root
    /// (`<repo>/glyphtrail.toml`).
    pub fn committed_config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    /// The gitignored personal config override (`.glyphtrail/glyphtrail.toml`).
    pub fn local_config_path(&self) -> PathBuf {
        self.index_dir.join(CONFIG_FILE)
    }

    /// The pre-unification analysis config (`.glyphtrail/config.toml`), still read
    /// for back-compat.
    pub fn legacy_config_path(&self) -> PathBuf {
        self.index_dir.join(LEGACY_CONFIG_FILE)
    }
}

/// Per-repo configuration, read from the unified `glyphtrail.toml` (committed)
/// overlaid by `.glyphtrail/glyphtrail.toml` (personal); see [`Config::load`].
/// Every field has a default, so a missing or partial file is always valid.
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
    /// Manual cross-repo link hints (#281): relationships the auto-resolver can't
    /// infer (a service called over HTTP with no shared package). Unioned across
    /// the committed and personal config files.
    pub links: Vec<LinkHint>,
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

/// Read a TOML file into a table, or `None` when it's absent.
fn read_table(path: &Path) -> crate::Result<Option<toml::Table>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let table = text
                .parse::<toml::Table>()
                .map_err(|source| CoreError::ConfigParse {
                    path: path.to_path_buf(),
                    source: Box::new(source),
                })?;
            Ok(Some(table))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::ConfigRead {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Take the `[[links]]` array out of a config table, returning its entries (so
/// links can be unioned across sources rather than overridden).
fn take_links(table: &mut toml::Table) -> Vec<toml::Value> {
    match table.remove("links") {
        Some(toml::Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// Overlay `over` onto `base`: nested tables merge recursively; every other
/// value (scalar or array) is replaced by `over`'s.
fn deep_merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(bt)), toml::Value::Table(ot)) => deep_merge(bt, ot),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

impl Config {
    /// Load the effective config for a repo: the committed `glyphtrail.toml`
    /// overlaid by the personal `.glyphtrail/glyphtrail.toml`, plus — for
    /// back-compat — the legacy `.glyphtrail/config.toml` and the standalone link
    /// files. Analysis settings override low→high (legacy < committed < personal);
    /// `[[links]]` from every source are unioned. Absent files are skipped, so a
    /// repo with no config is the default.
    pub fn load(repo_root: impl AsRef<Path>) -> crate::Result<Config> {
        let paths = RepoPaths::new(repo_root);
        let mut analysis = toml::Table::new();
        let mut links: Vec<toml::Value> = Vec::new();

        // Analysis config + any inline links, low→high precedence.
        for path in [
            paths.legacy_config_path(),    // legacy analysis (.glyphtrail/config.toml)
            paths.committed_config_path(), // committed (<repo>/glyphtrail.toml)
            paths.local_config_path(),     // personal (.glyphtrail/glyphtrail.toml)
        ] {
            if let Some(mut table) = read_table(&path)? {
                links.append(&mut take_links(&mut table));
                deep_merge(&mut analysis, table);
            }
        }
        // Legacy standalone link files (committed root + gitignored personal).
        for path in [
            paths.root.join(HINTS_FILE),
            paths.index_dir.join(LOCAL_HINTS_FILE),
        ] {
            if let Some(mut table) = read_table(&path)? {
                links.append(&mut take_links(&mut table));
            }
        }
        if !links.is_empty() {
            analysis.insert("links".to_string(), toml::Value::Array(links));
        }

        let text = toml::to_string(&analysis).unwrap_or_default();
        Config::parse(&text, &paths.committed_config_path())
    }

    pub fn from_toml_str(text: &str) -> crate::Result<Config> {
        Config::parse(text, Path::new(CONFIG_FILE))
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

    // #293: a pre-rename `.stratograph` index dir is silently moved to
    // `.glyphtrail` on first access, so an old index keeps working.
    #[test]
    fn repo_paths_migrates_legacy_index_dir() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("glyphtrail-migrate-{nanos}"));
        let legacy = root.join(LEGACY_INDEX_DIR);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("ladybug"), b"index").unwrap();

        let paths = RepoPaths::new(&root);
        check!(paths.index_dir == root.join(INDEX_DIR));
        check!(root.join(INDEX_DIR).join("ladybug").exists()); // moved
        check!(!legacy.exists());
        // With a current dir already present, the legacy one is left alone.
        std::fs::create_dir_all(&legacy).unwrap();
        RepoPaths::new(&root);
        check!(legacy.exists()); // not clobbered over the existing .glyphtrail
        std::fs::remove_dir_all(&root).ok();
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
