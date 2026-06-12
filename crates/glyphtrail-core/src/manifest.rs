//! Cargo manifest parsing for package identity (#220).
//!
//! Reads a `Cargo.toml` into a [`CargoPackage`]: the package name/version and
//! its declared dependencies, each classified by source (registry / git / path
//! / workspace-inherited). This is the producer/consumer identity the cross-repo
//! link step (#221) matches on: a consumer's dependency *name* ties to the
//! producer repo whose package name equals it, regardless of whether the
//! dependency is pulled from crates.io, a git URL, or a local path.
//!
//! Pure by design: this parses a manifest *string*. Locating `Cargo.toml` files
//! on disk and expanding a workspace's `members` globs is the analyze
//! integration's job, not this module's.

use serde::{Deserialize, Serialize};
use toml::Value;

use crate::{CoreError, Result};

/// Dependency category. A local crate can be pulled in as a normal, dev, or
/// build dependency; all three matter to cross-repo impact, since dev/build
/// deps drag the producer into a consumer's test and build code too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

/// Where a dependency resolves from, as declared in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepSource {
    /// A registry dependency (crates.io unless a `registry` key says otherwise).
    Registry,
    /// A git dependency; carries the repository URL.
    Git(String),
    /// A path dependency; carries the path relative to the manifest directory.
    Path(String),
    /// Inherited from the workspace (`foo.workspace = true`); the real source
    /// lives in the workspace-root manifest and is resolved later.
    Workspace,
}

/// One declared dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoDependency {
    /// The crate name to match against a producer's package name. This is the
    /// `package = "..."` rename target when present, otherwise the table key.
    pub name: String,
    /// The dependency table key, when it differs from `name` (a rename); the
    /// local name the consumer's code imports under. `None` when not renamed.
    pub alias: Option<String>,
    /// Version requirement string, when declared.
    pub req: Option<String>,
    pub kind: DepKind,
    pub source: DepSource,
}

/// A package declared by a `Cargo.toml`. A virtual / workspace-only manifest
/// (no `[package]` table, or a package with no `name`) parses to `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoPackage {
    pub name: String,
    pub version: Option<String>,
    /// `[package].description`, when declared.
    pub description: Option<String>,
    /// `[package].keywords`, lowercased — declared "what this is about" tags.
    #[serde(default)]
    pub keywords: Vec<String>,
    pub dependencies: Vec<CargoDependency>,
}

/// Parse a `Cargo.toml`'s text into a [`CargoPackage`]. Returns `Ok(None)` for a
/// manifest that declares no named package (a virtual workspace root), and an
/// error only when the TOML itself is malformed.
pub fn parse_cargo_manifest(text: &str) -> Result<Option<CargoPackage>> {
    let table: toml::Table = toml::from_str(text).map_err(|source| CoreError::ManifestParse {
        source: Box::new(source),
    })?;

    let Some(name) = table
        .get("package")
        .and_then(Value::as_table)
        .and_then(|pkg| pkg.get("name"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let version = table
        .get("package")
        .and_then(Value::as_table)
        .and_then(|pkg| pkg.get("version"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let description = table
        .get("package")
        .and_then(Value::as_table)
        .and_then(|pkg| pkg.get("description"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let keywords = table
        .get("package")
        .and_then(Value::as_table)
        .and_then(|pkg| pkg.get("keywords"))
        .and_then(Value::as_array)
        .map(|a| normalize_keywords(a.iter().filter_map(Value::as_str)))
        .unwrap_or_default();

    let mut dependencies = Vec::new();
    for (key, kind) in [
        ("dependencies", DepKind::Normal),
        ("dev-dependencies", DepKind::Dev),
        ("build-dependencies", DepKind::Build),
    ] {
        if let Some(deps) = table.get(key).and_then(Value::as_table) {
            for (dep_key, spec) in deps {
                dependencies.push(parse_dependency(dep_key, spec, kind));
            }
        }
    }

    Ok(Some(CargoPackage {
        name: name.to_string(),
        version,
        description,
        keywords,
        dependencies,
    }))
}

/// Classify a single dependency entry. Handles the shorthand `foo = "1"`, the
/// detailed `foo = { version = "1", git/path/workspace = ... }`, and the
/// `package = "real"` rename. `path`/`git` win over a co-declared `version` so a
/// pinned local dependency is still recognised as local.
fn parse_dependency(key: &str, spec: &Value, kind: DepKind) -> CargoDependency {
    match spec {
        // `foo = "1.2"` — a bare version requirement from a registry.
        Value::String(req) => CargoDependency {
            name: key.to_string(),
            alias: None,
            req: Some(req.clone()),
            kind,
            source: DepSource::Registry,
        },
        // `foo = { ... }` — a detailed spec.
        Value::Table(t) => {
            let renamed = t.get("package").and_then(Value::as_str);
            let name = renamed.unwrap_or(key).to_string();
            let alias = renamed.map(|_| key.to_string());
            let req = t.get("version").and_then(Value::as_str).map(str::to_string);
            let source = if let Some(git) = t.get("git").and_then(Value::as_str) {
                DepSource::Git(git.to_string())
            } else if let Some(path) = t.get("path").and_then(Value::as_str) {
                DepSource::Path(path.to_string())
            } else if t.get("workspace").and_then(Value::as_bool).unwrap_or(false) {
                DepSource::Workspace
            } else {
                DepSource::Registry
            };
            CargoDependency {
                name,
                alias,
                req,
                kind,
                source,
            }
        }
        // Any other TOML shape: a bare registry dependency with no requirement.
        _ => CargoDependency {
            name: key.to_string(),
            alias: None,
            req: None,
            kind,
            source: DepSource::Registry,
        },
    }
}

/// The `repository` URL a manifest declares: `[package].repository`, falling
/// back to `[workspace.package].repository` (the workspace-root form a member
/// inherits). `None` when neither is present or the TOML is malformed. Pure —
/// locating the `Cargo.toml` on disk is the caller's job (#378).
pub fn manifest_repository(text: &str) -> Option<String> {
    let table: toml::Table = toml::from_str(text).ok()?;
    let repository = |t: &Value| {
        t.as_table()
            .and_then(|tbl| tbl.get("repository"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    table.get("package").and_then(repository).or_else(|| {
        table
            .get("workspace")
            .and_then(Value::as_table)
            .and_then(|w| w.get("package"))
            .and_then(repository)
    })
}

/// The workspace `members` entries declared in a manifest's `[workspace]`
/// table, in declaration order. Each is a path or path glob (e.g. `crates/*`)
/// relative to the manifest directory; expanding the globs against the
/// filesystem is the caller's job. Empty when there is no `[workspace]` table.
pub fn workspace_members(text: &str) -> Vec<String> {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A package's identity as parsed from a non-Cargo manifest (#338 digest): the
/// declared description and external dependency names, plus the ecosystem tag.
/// Cargo has its own richer [`CargoPackage`]; convert via [`cargo_manifest_package`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestPackage {
    pub description: Option<String>,
    /// Declared package keywords (npm/pyproject), lowercased + deduped.
    pub keywords: Vec<String>,
    pub deps: Vec<String>,
    pub ecosystem: &'static str,
}

/// Normalize declared keyword/topic strings: trimmed, lowercased, non-empty,
/// de-duplicated (order-preserving), and capped — they ride into the embedding card.
pub fn normalize_keywords<'a>(it: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    it.map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.clone()))
        .take(20)
        .collect()
}

/// The external dependency names a Cargo package declares: `Normal`-kind deps that
/// are not local `path` dependencies (so workspace-inherited and registry/git crates
/// count, but sibling crates pulled by relative path are dropped as local noise).
pub fn cargo_external_deps(pkg: &CargoPackage) -> Vec<String> {
    let mut names: Vec<String> = pkg
        .dependencies
        .iter()
        .filter(|d| d.kind == DepKind::Normal && !matches!(d.source, DepSource::Path(_)))
        .map(|d| d.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Dependency names declared in a `[workspace.dependencies]` table (the external
/// crates a workspace root pins for its members), dropping local `path` entries.
pub fn workspace_dependencies(text: &str) -> Vec<String> {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    let Some(deps) = table
        .get("workspace")
        .and_then(Value::as_table)
        .and_then(|w| w.get("dependencies"))
        .and_then(Value::as_table)
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = deps
        .iter()
        .filter(|(_, spec)| {
            // Drop path deps; keep registry/git/workspace-version specs.
            !spec
                .as_table()
                .map(|t| t.contains_key("path"))
                .unwrap_or(false)
        })
        .map(|(key, spec)| {
            spec.as_table()
                .and_then(|t| t.get("package"))
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_string()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Parse an npm `package.json` for its description + external dependency names.
pub fn parse_npm_manifest(text: &str) -> Option<ManifestPackage> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let description = v
        .get("description")
        .and_then(|d| d.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut deps: Vec<String> = Vec::new();
    for key in ["dependencies", "peerDependencies", "optionalDependencies"] {
        if let Some(obj) = v.get(key).and_then(|d| d.as_object()) {
            deps.extend(obj.keys().cloned());
        }
    }
    deps.sort();
    deps.dedup();
    let keywords = v
        .get("keywords")
        .and_then(|k| k.as_array())
        .map(|a| normalize_keywords(a.iter().filter_map(|x| x.as_str())))
        .unwrap_or_default();
    (description.is_some() || !deps.is_empty() || !keywords.is_empty()).then_some(ManifestPackage {
        description,
        keywords,
        deps,
        ecosystem: "npm",
    })
}

/// Parse a Python `pyproject.toml` (PEP 621 `[project]` or `[tool.poetry]`).
pub fn parse_pyproject_manifest(text: &str) -> Option<ManifestPackage> {
    let table: toml::Table = toml::from_str(text).ok()?;
    let project = table
        .get("project")
        .or_else(|| table.get("tool").and_then(|t| t.get("poetry")))?;
    let description = project
        .get("description")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut deps: Vec<String> = Vec::new();
    if let Some(arr) = project.get("dependencies").and_then(Value::as_array) {
        // PEP 621: a list of PEP 508 strings (`requests>=2`).
        for d in arr {
            if let Some(name) = d.as_str().and_then(strip_pep508_name) {
                deps.push(name);
            }
        }
    } else if let Some(tab) = project.get("dependencies").and_then(Value::as_table) {
        // Poetry: a table; `python` is the interpreter constraint, not a package.
        deps.extend(tab.keys().filter(|k| k.as_str() != "python").cloned());
    }
    deps.sort();
    deps.dedup();
    let keywords = project
        .get("keywords")
        .and_then(Value::as_array)
        .map(|a| normalize_keywords(a.iter().filter_map(Value::as_str)))
        .unwrap_or_default();
    (description.is_some() || !deps.is_empty() || !keywords.is_empty()).then_some(ManifestPackage {
        description,
        keywords,
        deps,
        ecosystem: "pypi",
    })
}

/// The package name at the front of a PEP 508 requirement (`requests[extra]>=2`).
fn strip_pep508_name(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    let end = trimmed
        .find(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .unwrap_or(trimmed.len());
    let name = &trimmed[..end];
    (!name.is_empty()).then(|| name.to_string())
}

/// Parse the `require` directives of a Go `go.mod` for module dependency paths.
pub fn parse_gomod_manifest(text: &str) -> Option<ManifestPackage> {
    let mut deps = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("//") {
            continue;
        }
        if line.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block {
            if line == ")" {
                in_block = false;
            } else if let Some(name) = line.split_whitespace().next() {
                deps.push(name.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("require ")
            && let Some(name) = rest.split_whitespace().next()
        {
            deps.push(name.to_string());
        }
    }
    deps.sort();
    deps.dedup();
    (!deps.is_empty()).then_some(ManifestPackage {
        description: None,
        keywords: Vec::new(),
        deps,
        ecosystem: "go",
    })
}

/// Parse a PHP `composer.json` for its description + `require` names.
pub fn parse_composer_manifest(text: &str) -> Option<ManifestPackage> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let description = v
        .get("description")
        .and_then(|d| d.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let deps: Vec<String> = v
        .get("require")
        .and_then(|d| d.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let keywords = v
        .get("keywords")
        .and_then(|k| k.as_array())
        .map(|a| normalize_keywords(a.iter().filter_map(|x| x.as_str())))
        .unwrap_or_default();
    (description.is_some() || !deps.is_empty() || !keywords.is_empty()).then_some(ManifestPackage {
        description,
        keywords,
        deps,
        ecosystem: "composer",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn dep<'a>(pkg: &'a CargoPackage, name: &str) -> &'a CargoDependency {
        pkg.dependencies
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no dependency named {name}"))
    }

    #[test]
    fn parses_package_name_and_version() {
        let pkg = parse_cargo_manifest("[package]\nname = \"widget\"\nversion = \"1.2.3\"\n")
            .unwrap()
            .expect("named package");
        check!(pkg.name == "widget");
        check!(pkg.version == Some("1.2.3".to_string()));
        check!(pkg.dependencies.is_empty());
    }

    #[test]
    fn virtual_workspace_manifest_is_none() {
        let text = "[workspace]\nmembers = [\"crates/*\"]\n";
        check!(parse_cargo_manifest(text).unwrap() == None);
    }

    #[test]
    fn package_without_name_is_none() {
        check!(parse_cargo_manifest("[package]\nversion = \"0.1.0\"\n").unwrap() == None);
    }

    #[test]
    fn malformed_toml_is_an_error() {
        check!(parse_cargo_manifest("[package").is_err());
    }

    #[test]
    fn classifies_dependency_sources() {
        let text = r#"
            [package]
            name = "consumer"
            version = "0.1.0"

            [dependencies]
            reg = "1.0"
            detailed = { version = "2.0", features = ["x"] }
            gitdep = { git = "https://example.com/g.git" }
            localdep = { path = "../local" }
            inherited = { workspace = true }
        "#;
        let pkg = parse_cargo_manifest(text).unwrap().expect("named package");

        check!(dep(&pkg, "reg").source == DepSource::Registry);
        check!(dep(&pkg, "reg").req == Some("1.0".to_string()));
        check!(dep(&pkg, "detailed").source == DepSource::Registry);
        check!(dep(&pkg, "detailed").req == Some("2.0".to_string()));
        check!(dep(&pkg, "gitdep").source == DepSource::Git("https://example.com/g.git".into()));
        check!(dep(&pkg, "localdep").source == DepSource::Path("../local".into()));
        check!(dep(&pkg, "inherited").source == DepSource::Workspace);
    }

    #[test]
    fn path_wins_over_a_co_declared_version() {
        // A pinned local dependency must still be recognised as local.
        let text = r#"
            [package]
            name = "c"
            [dependencies]
            local = { path = "../local", version = "0.1" }
        "#;
        let pkg = parse_cargo_manifest(text).unwrap().unwrap();
        check!(dep(&pkg, "local").source == DepSource::Path("../local".into()));
        check!(dep(&pkg, "local").req == Some("0.1".to_string()));
    }

    #[test]
    fn resolves_package_rename_to_real_crate_name() {
        // `serde_yaml = { package = "serde_norway", ... }`: the real crate name
        // is the link key; the table key is recorded as the local alias.
        let text = r#"
            [package]
            name = "c"
            [dependencies]
            serde_yaml = { package = "serde_norway", version = "0.9" }
        "#;
        let pkg = parse_cargo_manifest(text).unwrap().unwrap();
        let d = dep(&pkg, "serde_norway");
        check!(d.name == "serde_norway");
        check!(d.alias == Some("serde_yaml".to_string()));
        check!(d.req == Some("0.9".to_string()));
    }

    #[test]
    fn records_dev_and_build_dependency_kinds() {
        let text = r#"
            [package]
            name = "c"
            [dependencies]
            runtime = "1"
            [dev-dependencies]
            testonly = "1"
            [build-dependencies]
            builder = "1"
        "#;
        let pkg = parse_cargo_manifest(text).unwrap().unwrap();
        check!(dep(&pkg, "runtime").kind == DepKind::Normal);
        check!(dep(&pkg, "testonly").kind == DepKind::Dev);
        check!(dep(&pkg, "builder").kind == DepKind::Build);
    }

    #[test]
    fn reads_workspace_members() {
        let text = "[workspace]\nmembers = [\"crates/*\", \"tools/cli\"]\n";
        check!(workspace_members(text) == vec!["crates/*".to_string(), "tools/cli".to_string()]);
    }

    #[test]
    fn no_workspace_table_yields_no_members() {
        check!(workspace_members("[package]\nname = \"x\"\n").is_empty());
    }

    #[test]
    fn reads_repository_from_package_then_workspace() {
        let pkg = "[package]\nname = \"x\"\nrepository = \"https://github.com/o/r\"\n";
        check!(manifest_repository(pkg) == Some("https://github.com/o/r".to_string()));

        let ws = "[workspace.package]\nrepository = \"https://gitlab.com/o/r\"\n";
        check!(manifest_repository(ws) == Some("https://gitlab.com/o/r".to_string()));

        // `[package]` wins over `[workspace.package]`.
        let both = "[package]\nname = \"x\"\nrepository = \"https://a/p\"\n\
                    [workspace.package]\nrepository = \"https://a/w\"\n";
        check!(manifest_repository(both) == Some("https://a/p".to_string()));

        check!(manifest_repository("[package]\nname = \"x\"\n") == None);
    }

    #[test]
    fn dotted_workspace_key_is_workspace_inherited() {
        // `serde.workspace = true` is the dotted-key form of the table spec.
        let text = "[package]\nname = \"c\"\n[dependencies]\nserde.workspace = true\n";
        let pkg = parse_cargo_manifest(text).unwrap().unwrap();
        check!(dep(&pkg, "serde").source == DepSource::Workspace);
    }

    #[test]
    fn cargo_description_and_external_deps() {
        let text = r#"
            [package]
            name = "widget"
            description = "  A widget toolkit  "
            [dependencies]
            anyhow = "1"
            serde.workspace = true
            sibling = { path = "../sibling" }
            [dev-dependencies]
            assert2 = "0.3"
        "#;
        let pkg = parse_cargo_manifest(text).unwrap().unwrap();
        check!(pkg.description.as_deref() == Some("A widget toolkit")); // trimmed
        // External = normal, non-path: anyhow (registry) + serde (workspace), not the
        // path sibling and not the dev-dep.
        check!(cargo_external_deps(&pkg) == vec!["anyhow".to_string(), "serde".to_string()]);
    }

    #[test]
    fn workspace_dependencies_drop_path_entries() {
        let text = "[workspace.dependencies]\nanyhow = \"1\"\nlocal = { path = \"crates/local\" }\n\
                    renamed = { package = \"real-crate\", version = \"2\" }\n";
        check!(
            workspace_dependencies(text) == vec!["anyhow".to_string(), "real-crate".to_string()]
        );
    }

    #[test]
    fn npm_merges_dep_groups_and_reads_description() {
        let text = r#"{ "description": "a tool", "dependencies": { "react": "^18" },
            "peerDependencies": { "react-dom": "^18" } }"#;
        let m = parse_npm_manifest(text).unwrap();
        check!(m.description.as_deref() == Some("a tool"));
        check!(m.deps == vec!["react".to_string(), "react-dom".to_string()]);
        check!(m.ecosystem == "npm");
    }

    #[test]
    fn keywords_parsed_normalized_across_ecosystems() {
        // Cargo: lowercased, trimmed, deduped, order-preserving.
        let cargo = parse_cargo_manifest(
            r#"[package]
               name = "w"
               keywords = ["CLI", " Cli ", "graph", "embeddings"]"#,
        )
        .unwrap()
        .unwrap();
        check!(
            cargo.keywords
                == vec![
                    "cli".to_string(),
                    "graph".to_string(),
                    "embeddings".to_string()
                ]
        );
        // npm.
        let npm = parse_npm_manifest(r#"{ "keywords": ["React", "ui"] }"#).unwrap();
        check!(npm.keywords == vec!["react".to_string(), "ui".to_string()]);
        // pyproject (PEP 621).
        let py = parse_pyproject_manifest("[project]\nname=\"p\"\nkeywords=[\"ml\",\"vision\"]\n")
            .unwrap();
        check!(py.keywords == vec!["ml".to_string(), "vision".to_string()]);
        // normalize_keywords standalone: dedup + cap + lowercase.
        check!(
            normalize_keywords(["A", "a", "b"].into_iter())
                == vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn pyproject_pep621_and_poetry() {
        let pep621 =
            "[project]\ndescription = \"svc\"\ndependencies = [\"requests>=2\", \"httpx\"]\n";
        let m = parse_pyproject_manifest(pep621).unwrap();
        check!(m.description.as_deref() == Some("svc"));
        check!(m.deps == vec!["httpx".to_string(), "requests".to_string()]); // sorted, PEP508 stripped
        // Poetry table form drops the `python` interpreter constraint.
        let poetry = "[tool.poetry]\ndescription = \"p\"\n[tool.poetry.dependencies]\npython = \"^3.11\"\nflask = \"^3\"\n";
        let m2 = parse_pyproject_manifest(poetry).unwrap();
        check!(m2.deps == vec!["flask".to_string()]);
    }

    #[test]
    fn gomod_require_block_and_single() {
        let text =
            "module x\n\nrequire github.com/a/b v1.2.3\n\nrequire (\n\tgithub.com/c/d v0.1.0\n)\n";
        let m = parse_gomod_manifest(text).unwrap();
        check!(m.deps == vec!["github.com/a/b".to_string(), "github.com/c/d".to_string()]);
    }
}
