//! Structured per-repo digest (#338 follow-up).
//!
//! Borrowed from the user's `codesearch` `repo-digest`: rather than embedding a
//! repo as `name + concatenated commit subjects`, build a compact, high-signal
//! document about what the repo *is* — languages, dependencies, API surface,
//! structure, git timeline, and topics — sourced from glyphtrail's own index plus
//! the atlas commit history. Used as the `atlas embed` text document (far better
//! repo representation) and surfaced by the `atlas digest` command.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{AtlasTimelineRow, NodeKind, derive_topics, format_date};
use glyphtrail_store::{GraphStore, LadybugStore};

/// A structured summary of one repository.
pub struct RepoDigest {
    pub name: String,
    /// The package description from a manifest, when declared — the most
    /// authoritative one-line "what is this repo".
    pub description: Option<String>,
    /// The forge's repo description (GitHub "about"), when known — the most
    /// authoritative "what is this" for a repo on a forge.
    pub forge_description: Option<String>,
    /// Declared package keywords (manifest) ∪ forge topics — curated "about" tags.
    pub keywords: Vec<String>,
    /// `(language, file count)`, descending.
    pub languages: Vec<(String, usize)>,
    pub total_files: usize,
    pub functions: usize,
    pub types: usize,
    pub endpoints: usize,
    /// A bounded sample of endpoint signatures (`GET /users`).
    pub top_endpoints: Vec<String>,
    /// Top external dependency / import roots.
    pub deps: Vec<String>,
    /// Salient topics from the commit history (fills codesearch's `concepts`).
    pub topics: Vec<String>,
    /// First paragraph of the README, bounded.
    pub readme: Option<String>,
    pub first_commit: Option<String>,
    pub last_commit: Option<String>,
    pub total_commits: usize,
    pub active_years: Vec<i32>,
}

/// Build a digest for a repo from its own analysis index (optional — pass its
/// working-tree `root`) and the atlas commit `rows` that belong to it (the caller
/// filters by repo). A missing root / unreadable index degrades gracefully: the
/// timeline + topics still come from the atlas commits.
pub fn build_repo_digest(
    name: &str,
    root: Option<&Path>,
    rows: &[&AtlasTimelineRow],
    forge: Option<&glyphtrail_core::RepoForgeMeta>,
) -> RepoDigest {
    let mut d = RepoDigest {
        name: name.to_string(),
        description: None,
        forge_description: forge.and_then(|f| f.description.clone()),
        keywords: forge.map(|f| f.topics.clone()).unwrap_or_default(),
        languages: Vec::new(),
        total_files: 0,
        functions: 0,
        types: 0,
        endpoints: 0,
        top_endpoints: Vec::new(),
        deps: Vec::new(),
        topics: Vec::new(),
        readme: root.and_then(read_readme),
        first_commit: None,
        last_commit: None,
        total_commits: rows.len(),
        active_years: Vec::new(),
    };

    // The repo's own index (read-only — never trigger a migration that would wipe an
    // out-of-date index, mirroring `graph-embed`).
    let repo_lb = root.map(|r| RepoPaths::new(r).index_dir.join("ladybug"));
    if let Some(repo_lb) = repo_lb.filter(|p| p.exists())
        && let Ok(store) = LadybugStore::open_read_only(&repo_lb)
    {
        if let Ok(stats) = store.stats() {
            d.languages = stats.languages;
            d.total_files = stats.files;
        }
        if let Ok(kinds) = store.node_kind_counts() {
            let count = |k: &str| {
                kinds
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, c)| *c)
                    .unwrap_or(0)
            };
            d.functions = count("function") + count("method");
            d.types = count("struct")
                + count("class")
                + count("interface")
                + count("enum")
                + count("trait");
        }
        if let Ok(eps) = store.operations_by_kind(NodeKind::Endpoint) {
            d.endpoints = eps.len();
            d.top_endpoints = eps.iter().take(12).map(|(_, k)| k.to_string()).collect();
        }
        if let Ok(imports) = store.all_imports() {
            // Frequency of import roots (crate / package / top module), most-used
            // first, dropping language-internal roots that carry no signal.
            const DEP_STOP: &[&str] = &[
                "std",
                "core",
                "alloc",
                "crate",
                "super",
                "self",
                "__future__",
            ];
            let mut freq: BTreeMap<String, usize> = BTreeMap::new();
            for (_importer, module, _lang) in imports {
                let root = module
                    .split(['/', ':', '.', '\\'])
                    .find(|s| !s.is_empty())
                    .unwrap_or(&module);
                if !root.is_empty() && !DEP_STOP.contains(&root) {
                    *freq.entry(root.to_string()).or_default() += 1;
                }
            }
            let mut v: Vec<(String, usize)> = freq.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            d.deps = v.into_iter().take(15).map(|(m, _)| m).collect();
        }
    }

    // Manifest layer (#338 follow-up, borrowed from codesearch): the declared
    // package description + external dependencies are more authoritative than
    // inferred imports — use them when a manifest is present.
    if let Some(root) = root {
        let (description, keywords, manifest_deps, _ecosystems) = repo_manifest_digest(root);
        d.description = description;
        // Manifest keywords ∪ forge topics (already seeded), deduped.
        d.keywords.extend(keywords);
        d.keywords = glyphtrail_core::normalize_keywords(d.keywords.iter().map(String::as_str));
        if !manifest_deps.is_empty() {
            d.deps = manifest_deps.into_iter().take(20).collect();
        }
    }

    // Timeline + topics from the atlas commits.
    let mut times: Vec<i64> = rows.iter().map(|r| r.commit.committed_at).collect();
    times.sort_unstable();
    d.first_commit = times.first().map(|t| format_date(*t));
    d.last_commit = times.last().map(|t| format_date(*t));
    let mut years: Vec<i32> = times.iter().filter_map(|t| year_of(*t)).collect();
    years.dedup();
    d.active_years = years;
    // Topics ranked by how often they recur across commits (not alphabetically),
    // so the salient themes surface.
    let mut topic_freq: BTreeMap<String, usize> = BTreeMap::new();
    for r in rows {
        for t in derive_topics(&r.commit.subject, &[]) {
            *topic_freq.entry(t).or_default() += 1;
        }
    }
    let mut topics: Vec<(String, usize)> = topic_freq.into_iter().collect();
    topics.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    d.topics = topics.into_iter().take(12).map(|(t, _)| t).collect();

    d
}

/// Aggregate the repo's package manifests into `(description, external deps,
/// ecosystems)`. Walks the working tree (bounded) for `Cargo.toml`/`package.json`/
/// `pyproject.toml`/`go.mod`/`composer.json`, parses each with the core manifest
/// parsers, and unions the declared external dependencies. The description is the
/// shallowest manifest's (root wins over a member crate). #338.
type ParsedManifest = (
    Option<String>, // description
    Vec<String>,    // keywords
    Vec<String>,    // external deps
    &'static str,   // ecosystem
    Option<String>, // own package name (workspace self-dep subtraction)
);

fn repo_manifest_digest(root: &Path) -> (Option<String>, Vec<String>, Vec<String>, Vec<String>) {
    use glyphtrail_core::{
        cargo_external_deps, parse_cargo_manifest, parse_composer_manifest, parse_gomod_manifest,
        parse_npm_manifest, parse_pyproject_manifest, workspace_dependencies,
    };
    let mut manifests: Vec<(usize, std::path::PathBuf)> = Vec::new();
    collect_manifests(root, 0, &mut manifests);
    manifests.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1))); // shallowest (root) first
    let mut description: Option<String> = None;
    let mut keywords: Vec<String> = Vec::new();
    let mut deps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut ecosystems: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // The repo's own package names — these leak into a member's deps via
    // `foo.workspace = true`, so subtract them so a workspace doesn't list itself.
    let mut local: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (depth, path) in &manifests {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let parsed: Option<ParsedManifest> =
            match path.file_name().and_then(|n| n.to_str()).unwrap_or("") {
                "Cargo.toml" => match parse_cargo_manifest(&text) {
                    Ok(Some(pkg)) => {
                        let mut d = cargo_external_deps(&pkg);
                        d.extend(workspace_dependencies(&text));
                        Some((
                            pkg.description.clone(),
                            pkg.keywords.clone(),
                            d,
                            "cargo",
                            Some(pkg.name.clone()),
                        ))
                    }
                    // A virtual workspace root: still mine its `[workspace.dependencies]`.
                    _ => {
                        let ws = workspace_dependencies(&text);
                        (!ws.is_empty()).then_some((None, Vec::new(), ws, "cargo", None))
                    }
                },
                "package.json" => parse_npm_manifest(&text)
                    .map(|m| (m.description, m.keywords, m.deps, m.ecosystem, None)),
                "pyproject.toml" => parse_pyproject_manifest(&text)
                    .map(|m| (m.description, m.keywords, m.deps, m.ecosystem, None)),
                "go.mod" => parse_gomod_manifest(&text)
                    .map(|m| (m.description, m.keywords, m.deps, m.ecosystem, None)),
                "composer.json" => parse_composer_manifest(&text)
                    .map(|m| (m.description, m.keywords, m.deps, m.ecosystem, None)),
                _ => None,
            };
        if let Some((desc, kw, mdeps, eco, own)) = parsed {
            // Only the root manifest's description is repo-level; a member crate's
            // is too narrow, so a workspace falls back to its README.
            if *depth == 0
                && description.is_none()
                && let Some(s) = desc
            {
                description = Some(s);
            }
            keywords.extend(kw);
            deps.extend(mdeps);
            ecosystems.insert(eco.to_string());
            if let Some(n) = own {
                local.insert(n);
            }
        }
    }
    deps.retain(|d| !local.contains(d));
    let keywords = glyphtrail_core::normalize_keywords(keywords.iter().map(String::as_str));
    (
        description,
        keywords,
        deps.into_iter().collect(),
        ecosystems.into_iter().collect(),
    )
}

/// Collect manifest file paths under `dir` (bounded depth), each tagged with its
/// depth, skipping vendored / build / VCS directories.
fn collect_manifests(dir: &Path, depth: usize, out: &mut Vec<(usize, std::path::PathBuf)>) {
    if depth > 3 {
        return;
    }
    const SKIP: &[&str] = &[
        "target",
        "node_modules",
        ".git",
        ".glyphtrail",
        "vendor",
        "dist",
        "build",
        ".venv",
        "__pycache__",
    ];
    const MANIFESTS: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "composer.json",
    ];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or("");
        let path = entry.path();
        if path.is_dir() {
            if !SKIP.contains(&name) && !name.starts_with('.') {
                collect_manifests(&path, depth + 1, out);
            }
        } else if MANIFESTS.contains(&name) {
            out.push((depth, path));
        }
    }
}

/// The compact document fed to the text embedder: a few high-signal lines, bounded
/// so it stays cheap to embed. Includes a topics line so the "what was worked on"
/// commit signal is preserved alongside the structure.
pub fn render_embed_doc(d: &RepoDigest) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{}", d.name);
    // What-is-this, most authoritative first: the forge "about", the package
    // description, then the README summary — each included only when it adds signal
    // (skip a line a prior one already covers, since the README often repeats the
    // description).
    let mut said: Vec<String> = Vec::new();
    for cand in [
        d.forge_description.as_deref(),
        d.description.as_deref(),
        d.readme.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let c = cand.trim();
        let lc = c.to_lowercase();
        if c.is_empty()
            || said
                .iter()
                .any(|p| p.contains(&lc) || lc.contains(p.as_str()))
        {
            continue;
        }
        let _ = writeln!(s, "{c}");
        said.push(lc);
    }
    if !d.keywords.is_empty() {
        let _ = writeln!(s, "Keywords: {}", d.keywords.join(", "));
    }
    if !d.languages.is_empty() {
        let langs = d
            .languages
            .iter()
            .take(6)
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(s, "Languages: {langs}");
    }
    if !d.deps.is_empty() {
        let _ = writeln!(s, "Dependencies: {}", d.deps.join(", "));
    }
    if d.endpoints > 0 {
        let _ = writeln!(
            s,
            "API: {} endpoints — {}",
            d.endpoints,
            d.top_endpoints.join(", ")
        );
    }
    let _ = writeln!(
        s,
        "Structure: {} functions, {} types, {} files",
        d.functions, d.types, d.total_files
    );
    if d.total_commits > 0 {
        let years = d
            .active_years
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        // Lead with the era span so the historical context the work was made in is a
        // salient signal in the embedding, then the exact dates + active years.
        let era = match (d.active_years.first(), d.active_years.last()) {
            (Some(a), Some(b)) if a != b => format!("{a}–{b}"),
            (Some(a), _) => a.to_string(),
            _ => "?".to_string(),
        };
        let _ = writeln!(
            s,
            "Era: {era}; {} commits {}..{} (active {years})",
            d.total_commits,
            d.first_commit.as_deref().unwrap_or("?"),
            d.last_commit.as_deref().unwrap_or("?"),
        );
    }
    if !d.topics.is_empty() {
        let _ = writeln!(s, "Topics: {}", d.topics.join(", "));
    }
    s
}

/// Fuller Markdown rendering for `atlas digest` (mirrors codesearch's `digest.md`).
pub fn render_markdown(d: &RepoDigest) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {}", d.name);
    if let Some(first) = &d.first_commit {
        let _ = writeln!(
            s,
            "\n> {} commits · {}..{} · active {}\n",
            d.total_commits,
            first,
            d.last_commit.as_deref().unwrap_or("?"),
            d.active_years
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if let Some(desc) = &d.forge_description {
        let _ = writeln!(s, "> {desc}\n");
    }
    if let Some(desc) = &d.description {
        let _ = writeln!(s, "> {desc}\n");
    }
    if !d.keywords.is_empty() {
        let _ = writeln!(s, "**Keywords:** {}\n", d.keywords.join(", "));
    }
    if let Some(readme) = &d.readme {
        let _ = writeln!(s, "{readme}\n");
    }
    if !d.languages.is_empty() {
        let _ = writeln!(s, "## Languages\n");
        let total = d.total_files.max(1) as f64;
        for (lang, files) in &d.languages {
            let _ = writeln!(
                s,
                "- **{lang}** — {files} files ({:.0}%)",
                *files as f64 / total * 100.0
            );
        }
        let _ = writeln!(s);
    }
    let _ = writeln!(
        s,
        "## Structure\n\n- {} functions · {} types · {} files · {} endpoints\n",
        d.functions, d.types, d.total_files, d.endpoints
    );
    if !d.top_endpoints.is_empty() {
        let _ = writeln!(s, "- API: `{}`\n", d.top_endpoints.join("`, `"));
    }
    if !d.deps.is_empty() {
        let _ = writeln!(s, "## Dependencies\n\n- `{}`\n", d.deps.join("`, `"));
    }
    if !d.topics.is_empty() {
        let _ = writeln!(s, "## Topics\n\n- {}\n", d.topics.join(", "));
    }
    s
}

/// JSON rendering for `atlas digest --json`.
pub fn render_json(d: &RepoDigest) -> serde_json::Value {
    serde_json::json!({
        "name": d.name,
        "description": d.description,
        "forge_description": d.forge_description,
        "keywords": d.keywords,
        "languages": d.languages.iter().map(|(l, c)| serde_json::json!({ "name": l, "files": c })).collect::<Vec<_>>(),
        "total_files": d.total_files,
        "functions": d.functions,
        "types": d.types,
        "endpoints": d.endpoints,
        "top_endpoints": d.top_endpoints,
        "dependencies": d.deps,
        "topics": d.topics,
        "readme": d.readme,
        "first_commit": d.first_commit,
        "last_commit": d.last_commit,
        "total_commits": d.total_commits,
        "active_years": d.active_years,
    })
}

/// The year of a unix-seconds timestamp, via `format_date` (`YYYY-MM-DD`).
fn year_of(secs: i64) -> Option<i32> {
    format_date(secs).get(0..4).and_then(|y| y.parse().ok())
}

/// The README's first paragraph (up to a blank line), whitespace-collapsed and
/// bounded, or `None` if there's no README.
fn read_readme(root: &Path) -> Option<String> {
    let path = ["README.md", "README", "README.txt", "readme.md"]
        .into_iter()
        .map(|n| root.join(n))
        .find(|p| p.exists())?;
    let text = std::fs::read_to_string(&path).ok()?;
    // The first paragraph that is actual prose — skip headings, HTML blocks (hero
    // images, `<p align=…>`), badge/link lines, tables, and code fences.
    let is_prose = |p: &&str| {
        let p = p.trim();
        !p.is_empty()
            && !p.starts_with(['#', '<', '[', '!', '|'])
            && !p.starts_with("```")
            && p.chars().any(char::is_alphabetic)
    };
    let para = text.split("\n\n").map(str::trim).find(is_prose)?;
    let collapsed = para.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate_chars(&collapsed, 400))
}

/// Truncate to at most `max` chars on a char boundary, adding `…` if cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn digest() -> RepoDigest {
        RepoDigest {
            name: "codegraph".into(),
            description: Some("Local code intelligence graph.".into()),
            forge_description: Some("Code intelligence for AI coding agents.".into()),
            keywords: vec!["code-graph".into(), "embeddings".into()],
            languages: vec![("Rust".into(), 120), ("TOML".into(), 8)],
            total_files: 128,
            functions: 400,
            types: 60,
            endpoints: 2,
            top_endpoints: vec!["GET /a".into(), "POST /b".into()],
            deps: vec!["lbug".into(), "clap".into()],
            topics: vec!["embeddings".into(), "atlas".into()],
            readme: Some("Local code intelligence graph for AI agents.".into()),
            first_commit: Some("2025-01-02".into()),
            last_commit: Some("2026-06-06".into()),
            total_commits: 300,
            active_years: vec![2025, 2026],
        }
    }

    #[test]
    fn embed_doc_is_compact_and_high_signal() {
        let doc = render_embed_doc(&digest());
        check!(doc.contains("codegraph"));
        check!(doc.contains("Code intelligence for AI coding agents.")); // forge "about" leads
        check!(doc.contains("Local code intelligence graph.")); // manifest description too
        check!(doc.contains("Keywords: code-graph, embeddings")); // manifest kw ∪ forge topics
        check!(doc.contains("Languages: Rust, TOML"));
        check!(doc.contains("Dependencies: lbug, clap"));
        check!(doc.contains("API: 2 endpoints"));
        check!(doc.contains("Topics: embeddings, atlas"));
        check!(doc.contains("Era: 2025–2026")); // historical context, explicit span
        // Bounded — a card, not a commit dump.
        check!(doc.lines().count() < 14);
    }

    #[test]
    fn year_of_parses_timestamp() {
        // 2021-01-01T00:00:00Z = 1609459200
        check!(year_of(1_609_459_200) == Some(2021));
    }

    #[test]
    fn readme_excerpt_truncates() {
        let long = "word ".repeat(200);
        let t = truncate_chars(&long, 400);
        check!(t.chars().count() == 401); // 400 + the ellipsis
        check!(t.ends_with('…'));
    }
}
