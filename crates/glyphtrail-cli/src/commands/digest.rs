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
) -> RepoDigest {
    let mut d = RepoDigest {
        name: name.to_string(),
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

/// The compact document fed to the text embedder: a few high-signal lines, bounded
/// so it stays cheap to embed. Includes a topics line so the "what was worked on"
/// commit signal is preserved alongside the structure.
pub fn render_embed_doc(d: &RepoDigest) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{}", d.name);
    if let Some(readme) = &d.readme {
        let _ = writeln!(s, "{readme}");
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
        let _ = writeln!(
            s,
            "Activity: {} commits, {}..{} (active {years})",
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
        check!(doc.contains("Languages: Rust, TOML"));
        check!(doc.contains("Dependencies: lbug, clap"));
        check!(doc.contains("API: 2 endpoints"));
        check!(doc.contains("Topics: embeddings, atlas"));
        check!(doc.contains("active 2025, 2026"));
        // Bounded — a few lines, not a commit dump.
        check!(doc.lines().count() < 12);
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
