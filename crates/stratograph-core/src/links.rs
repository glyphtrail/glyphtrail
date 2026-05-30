//! Cross-repo link resolution (#221): connect a consumer's external use-site to
//! the producer crate's exported symbol, across the package boundary.
//!
//! Pure: given each repo's [`PackageIdentity`] (the producer `packages` with
//! their exports and the consumer `external_uses`), match every external use to
//! the producer repo whose package name equals the referenced crate. Matching
//! is **symbol-level with a crate-level fallback**: the imported symbol name is
//! matched against the producer's exports; when no export matches (a `pub use`
//! re-export, a macro-generated item, or a parser gap) the link falls back to
//! the producer package as a whole, so a known dependency is never dropped.
//!
//! Self-repo uses (a workspace member depending on a sibling) are skipped here:
//! within one repo the intra-repo call/import edges already capture the impact,
//! and this layer exists for the *cross*-repo boundary. The federated impact
//! traversal (#222) consumes these links as `ExternUses` edges.

use std::collections::HashMap;

use serde::Serialize;

use crate::PackageIdentity;

/// How precisely a cross-repo link resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// Matched a specific exported symbol of the producer.
    Symbol,
    /// No symbol matched; linked to the producer package as a whole.
    Crate,
}

/// One resolved cross-repo edge from a consumer use-site to a producer export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrossRepoLink {
    pub from_repo: String,
    pub from_package: String,
    pub from_file: String,
    pub to_repo: String,
    pub to_package: String,
    /// Matched export symbol name (symbol-level), or `None` for a crate link.
    pub to_symbol: Option<String>,
    /// Matched export node id (symbol-level), or `None` for a crate link.
    pub to_node: Option<String>,
    /// The consumer import path that produced this link.
    pub path: String,
    pub kind: LinkKind,
    /// Consumer symbol node ids that reference the import — the precise
    /// use-sites (#236). Empty falls back to file-level landing.
    pub from_nodes: Vec<String>,
}

/// A repo's name paired with its persisted package identity, the input to
/// [`resolve_links`].
#[derive(Debug, Clone)]
pub struct RepoIdentity {
    pub repo: String,
    pub identity: PackageIdentity,
}

/// Resolve every cross-repo link across `repos`. Deterministic: links are
/// sorted and de-duplicated.
pub fn resolve_links(repos: &[RepoIdentity]) -> Vec<CrossRepoLink> {
    // Producer index: crate name -> [(repo index, package index)].
    let mut producers: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    for (ri, r) in repos.iter().enumerate() {
        for (pi, p) in r.identity.packages.iter().enumerate() {
            producers
                .entry(p.package.name.as_str())
                .or_default()
                .push((ri, pi));
        }
    }

    let mut links = Vec::new();
    for (ci, consumer) in repos.iter().enumerate() {
        for u in &consumer.identity.external_uses {
            let Some(prods) = producers.get(u.package.as_str()) else {
                continue; // dependency isn't produced by any repo in the set
            };
            let symbols = imported_symbols(&u.path);
            for &(ri, pi) in prods {
                if ri == ci {
                    continue; // same repo — covered by intra-repo edges
                }
                let to_repo = repos[ri].repo.clone();
                let producer = &repos[ri].identity.packages[pi];

                let base = |to_symbol, to_node, kind| CrossRepoLink {
                    from_repo: consumer.repo.clone(),
                    from_package: u.from_package.clone(),
                    from_file: u.from_file.clone(),
                    to_repo: to_repo.clone(),
                    to_package: u.package.clone(),
                    to_symbol,
                    to_node,
                    path: u.path.clone(),
                    kind,
                    from_nodes: u.from_nodes.clone(),
                };

                let mut matched = false;
                for sym in &symbols {
                    for e in producer.exports.iter().filter(|e| &e.name == sym) {
                        links.push(base(
                            Some(sym.clone()),
                            Some(e.node_id.clone()),
                            LinkKind::Symbol,
                        ));
                        matched = true;
                    }
                }
                if !matched {
                    links.push(base(None, None, LinkKind::Crate));
                }
            }
        }
    }

    links.sort_by(|a, b| {
        (
            &a.from_repo,
            &a.from_file,
            &a.to_repo,
            &a.to_package,
            &a.to_symbol,
        )
            .cmp(&(
                &b.from_repo,
                &b.from_file,
                &b.to_repo,
                &b.to_package,
                &b.to_symbol,
            ))
    });
    links.dedup();
    links
}

/// The symbol name(s) an import path references, ignoring the leading crate
/// segment. `widget::go` → `[go]`; `widget::sub::Thing` → `[Thing]`;
/// `widget::{a, b as c}` → `[a, b]`; a bare `widget`, a glob `widget::*`, or a
/// `self` import → `[]` (crate-level).
pub fn imported_symbols(path: &str) -> Vec<String> {
    let path = path.trim().trim_start_matches("::");
    if let Some(open) = path.find('{') {
        let close = path.rfind('}').unwrap_or(path.len());
        return path[open + 1..close]
            .split(',')
            .filter_map(|item| {
                let item = item.split(" as ").next().unwrap_or(item).trim();
                item.rsplit("::").next()
            })
            .filter(|s| !s.is_empty() && *s != "self" && *s != "*")
            .map(str::to_string)
            .collect();
    }
    if !path.contains("::") {
        return Vec::new(); // crate root only, no symbol tail
    }
    match path.rsplit("::").next() {
        Some(seg) if !seg.is_empty() && seg != "*" && seg != "self" => vec![seg.to_string()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExternalUse, IndexedPackage, NodeKind, PackageExport, parse_cargo_manifest};
    use assert2::check;

    fn pkg(name: &str, exports: &[&str]) -> IndexedPackage {
        IndexedPackage {
            dir: String::new(),
            package: parse_cargo_manifest(&format!("[package]\nname = \"{name}\"\n"))
                .unwrap()
                .unwrap(),
            exports: exports
                .iter()
                .map(|n| PackageExport {
                    name: (*n).to_string(),
                    qualified_name: (*n).to_string(),
                    kind: NodeKind::Function,
                    file: format!("src/{n}.rs"),
                    node_id: format!("node-{n}"),
                })
                .collect(),
        }
    }

    fn uses(from_package: &str, dep: &str, path: &str) -> ExternalUse {
        ExternalUse {
            from_package: from_package.to_string(),
            from_file: format!("{from_package}/src/lib.rs"),
            package: dep.to_string(),
            path: path.to_string(),
            from_nodes: Vec::new(),
        }
    }

    fn repo(
        name: &str,
        packages: Vec<IndexedPackage>,
        external_uses: Vec<ExternalUse>,
    ) -> RepoIdentity {
        RepoIdentity {
            repo: name.to_string(),
            identity: PackageIdentity {
                packages,
                external_uses,
            },
        }
    }

    #[test]
    fn symbol_level_link_to_a_matching_export() {
        let producer = repo("widget-repo", vec![pkg("widget", &["go", "stop"])], vec![]);
        let consumer = repo(
            "app-repo",
            vec![pkg("app", &[])],
            vec![uses("app", "widget", "widget::go")],
        );
        let links = resolve_links(&[producer, consumer]);
        check!(links.len() == 1);
        check!(links[0].kind == LinkKind::Symbol);
        check!(links[0].to_repo == "widget-repo");
        check!(links[0].to_symbol == Some("go".to_string()));
        check!(links[0].to_node == Some("node-go".to_string()));
    }

    #[test]
    fn crate_level_fallback_when_no_export_matches() {
        // `widget::mystery` isn't an export (e.g. a pub-use re-export we can't
        // resolve) -> crate-level link, never dropped.
        let producer = repo("widget-repo", vec![pkg("widget", &["go"])], vec![]);
        let consumer = repo(
            "app-repo",
            vec![pkg("app", &[])],
            vec![uses("app", "widget", "widget::mystery")],
        );
        let links = resolve_links(&[producer, consumer]);
        check!(links.len() == 1);
        check!(links[0].kind == LinkKind::Crate);
        check!(links[0].to_symbol == None);
    }

    #[test]
    fn use_list_links_each_named_symbol() {
        let producer = repo("w", vec![pkg("widget", &["a", "b", "c"])], vec![]);
        let consumer = repo(
            "app",
            vec![pkg("app", &[])],
            vec![uses("app", "widget", "widget::{a, b as bb}")],
        );
        let links = resolve_links(&[producer, consumer]);
        let syms: Vec<_> = links.iter().filter_map(|l| l.to_symbol.clone()).collect();
        check!(syms == vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn self_repo_use_is_skipped() {
        // A workspace where `app` uses sibling `widget` in the SAME repo: no
        // cross-repo link (intra-repo edges already cover it).
        let mono = repo(
            "mono",
            vec![pkg("widget", &["go"]), pkg("app", &[])],
            vec![uses("app", "widget", "widget::go")],
        );
        check!(resolve_links(&[mono]).is_empty());
    }

    #[test]
    fn unproduced_dependency_yields_no_link() {
        // `serde` is depended on but produced by no repo in the set.
        let consumer = repo(
            "app",
            vec![pkg("app", &[])],
            vec![uses("app", "serde", "serde::Serialize")],
        );
        check!(resolve_links(&[consumer]).is_empty());
    }

    #[test]
    fn imported_symbols_parses_paths() {
        check!(imported_symbols("widget::go") == vec!["go".to_string()]);
        check!(imported_symbols("widget::sub::Thing") == vec!["Thing".to_string()]);
        check!(imported_symbols("widget") == Vec::<String>::new());
        check!(imported_symbols("widget::*") == Vec::<String>::new());
        check!(imported_symbols("widget::{a, b as c}") == vec!["a".to_string(), "b".to_string()]);
    }
}
