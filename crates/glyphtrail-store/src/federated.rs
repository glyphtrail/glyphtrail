//! Cross-repo blast-radius orchestration (#222 / #223).
//!
//! Ties the registry, the persisted package identity, cross-repo link
//! resolution and the [`FederatedAdjacency`] together into one entry point,
//! [`federated_impact`], shared by the `glyphtrail impact --downstream` CLI and
//! the `impact` MCP tool. It resolves cross-repo links from the identities cached
//! on the registry (#292), seeds in the current repo, opens only the member
//! stores reachable across the package boundary, traverses, and returns a
//! [`FederatedReport`] (origin repo first, then downstream).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{
    Adjacency, ClassifiedItem, Confidence, CrateLevelHit, FederatedAdjacency, FederatedReport,
    Groups, ImpactPolicy, IndexedPackage, META_EXTERNAL_USES, META_PACKAGES, NodeId, NodeKind,
    PackageIdentity, Registry, RepoHealth, RepoIdentity, RepoImpact, classify, compute_impact,
    default_groups_path, default_registry_path, is_cross_boundary_path, qualify, resolve_links,
    unqualify,
};

use crate::{ChangeSpec, GraphStore, LadybugStore, changed_files, seed_nodes};

/// Which repos a federated query spans.
pub enum FederationScope {
    /// Every repo in the global registry.
    Registry,
    /// Only the members of a named group.
    Group(String),
}

/// How to seed the federated traversal, resolved against the current repo.
pub enum SeedSpec {
    /// Every definition matching a symbol name.
    Name(String),
    /// Every symbol in a git change set (file/files/since/staged/diff).
    Change(ChangeSpec),
}

/// Whether a node is a definition worth landing cross-repo impact on, so the
/// cross hop reaches real symbols that propagate (not files or comments).
fn is_symbol_node(kind: NodeKind) -> bool {
    !matches!(
        kind,
        NodeKind::Repo | NodeKind::Directory | NodeKind::File | NodeKind::Comment
    )
}

/// The package that owns `file`: the one whose directory is the longest matching
/// prefix (an empty dir is the repo root, matching anything but losing to a
/// deeper dir). Returns the package name.
fn owning_package<'a>(packages: &'a [IndexedPackage], file: &str) -> Option<&'a str> {
    packages
        .iter()
        .filter(|p| p.dir.is_empty() || file == p.dir || file.starts_with(&format!("{}/", p.dir)))
        .max_by_key(|p| p.dir.len())
        .map(|p| p.name.as_str())
}

/// Compute the cross-repo blast radius: seed in the repo at `current_root` and
/// traverse into downstream repos across the link table, scoped to the whole
/// registry or a named group. The current repo must be registered and indexed.
///
/// `deep` trades speed for thoroughness when the registry shortcut may be
/// incomplete (indexing the dependency before its dependents, or a cache write
/// skipped under lock contention): instead of trusting the identities cached on
/// the registry, it re-reads each member's identity from its store, and it
/// discovers indexed repos sitting beside the current one that were never
/// `repo add`ed and folds them in. The default (`deep = false`) is the fast path.
pub fn federated_impact(
    current_root: &Path,
    scope: &FederationScope,
    seeds: SeedSpec,
    policy: &ImpactPolicy,
    deep: bool,
) -> Result<FederatedReport> {
    let registry = Registry::load(
        &default_registry_path().ok_or_else(|| anyhow!("cannot locate home directory"))?,
    )?;
    let here = current_root
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", current_root.display()))?;
    let current = registry
        .repos
        .iter()
        .find(|e| {
            e.roots()
                .any(|root| root.canonicalize().map(|r| r == here).unwrap_or(false))
        })
        .map(|e| e.name.clone())
        .ok_or_else(|| {
            anyhow!(
                "repo {} is not registered — `glyphtrail repo add` it to federate",
                here.display()
            )
        })?;

    // Repos in scope; the current repo is always included so its seeds traverse.
    let mut names: Vec<String> = match scope {
        FederationScope::Registry => registry.repos.iter().map(|e| e.name.clone()).collect(),
        FederationScope::Group(g) => Groups::load(
            &default_groups_path().ok_or_else(|| anyhow!("cannot locate home directory"))?,
        )?
        .get(g)
        .ok_or_else(|| anyhow!("no group named '{g}'"))?
        .repos
        .clone(),
    };
    if !names.contains(&current) {
        names.push(current.clone());
    }

    // Cross-repo identities come from the registry (#292), so we don't open a
    // store just to read identity. An entry indexed before this cache existed
    // has `identity: None`; for those (and for every member under `deep`) we open
    // the store, read its identity fresh, and best-effort backfill the registry,
    // keeping the opened store to reuse below.
    let mut prefetched: HashMap<String, Box<dyn GraphStore>> = HashMap::new();
    let mut backfills: Vec<(std::path::PathBuf, PackageIdentity)> = Vec::new();
    let mut identities: Vec<RepoIdentity> = Vec::new();
    for name in &names {
        let Some(entry) = registry.get(name) else {
            continue;
        };
        if entry.health() != RepoHealth::Indexed {
            continue;
        }
        let identity = if !deep && let Some(id) = &entry.identity {
            id.clone()
        } else {
            // Deep mode, or no cached identity: read straight from the store.
            let ladybug = RepoPaths::new(entry.active_root())
                .index_dir
                .join("ladybug");
            match LadybugStore::open(&ladybug) {
                Ok(store) => {
                    let id = PackageIdentity::from_meta(
                        store.get_meta(META_PACKAGES).ok().flatten().as_deref(),
                        store.get_meta(META_EXTERNAL_USES).ok().flatten().as_deref(),
                    );
                    backfills.push((entry.active_root().clone(), id.clone()));
                    prefetched.insert(name.clone(), Box::new(store));
                    id
                }
                // One unreadable member shouldn't sink the run.
                Err(e) => {
                    eprintln!("note: skipping repo '{name}': cannot open its index ({e})");
                    continue;
                }
            }
        };
        identities.push(RepoIdentity {
            repo: name.clone(),
            identity,
        });
    }

    // Deep scan: discover indexed repos sitting beside the current one that were
    // never registered, so a dependent indexed-but-not-`repo add`ed is still
    // found (#292 follow-up). Bounded to the immediate siblings of the current
    // repo; keyed by directory name and skipped on a name clash with a member.
    if deep
        && let Some(parent) = here.parent()
        && let Ok(dir_entries) = std::fs::read_dir(parent)
    {
        let registered: HashSet<std::path::PathBuf> = registry
            .repos
            .iter()
            .flat_map(|e| e.roots().filter_map(|r| r.canonicalize().ok()))
            .collect();
        for dir_entry in dir_entries.flatten() {
            let dir = dir_entry.path();
            let ladybug = RepoPaths::new(&dir).index_dir.join("ladybug");
            if !dir.is_dir() || !ladybug.exists() {
                continue;
            }
            if dir
                .canonicalize()
                .ok()
                .is_some_and(|c| registered.contains(&c))
            {
                continue; // already a registry member
            }
            let Some(name) = dir.file_name().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            if name.is_empty() || identities.iter().any(|i| i.repo == name) {
                continue;
            }
            if let Ok(store) = LadybugStore::open(&ladybug) {
                let id = PackageIdentity::from_meta(
                    store.get_meta(META_PACKAGES).ok().flatten().as_deref(),
                    store.get_meta(META_EXTERNAL_USES).ok().flatten().as_deref(),
                );
                prefetched.insert(name.clone(), Box::new(store));
                identities.push(RepoIdentity {
                    repo: name,
                    identity: id,
                });
            }
        }
    }

    // Best-effort: cache (re)read identities so the next query is cheap (#292),
    // which also self-heals a stale cache on a deep run. Lock-tolerant — never
    // fail the query because the registry is busy.
    if !backfills.is_empty()
        && let Some(reg_path) = default_registry_path()
    {
        let _ = Registry::mutate(&reg_path, |r| {
            for (root, id) in &backfills {
                r.set_identity_by_root(root, id.clone());
            }
        });
    }

    let links = resolve_links(&identities);

    // Only repos reachable from the current repo along producer -> consumer edges
    // (a consumer's external use of a producer is a link `to_repo -> from_repo`)
    // can receive impact from its seeds, so only those need their store opened —
    // not the whole registry. BFS the repo-level link graph from the current repo.
    let mut consumers_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for l in &links {
        consumers_of
            .entry(l.to_repo.as_str())
            .or_default()
            .push(l.from_repo.as_str());
    }
    let mut reachable: HashSet<String> = HashSet::from([current.clone()]);
    let mut queue = vec![current.clone()];
    while let Some(r) = queue.pop() {
        if let Some(consumers) = consumers_of.get(r.as_str()) {
            for &c in consumers {
                if reachable.insert(c.to_string()) {
                    queue.push(c.to_string());
                }
            }
        }
    }

    // Open the stores for just the reachable repos, reusing any opened during
    // backfill. A connected member whose index won't open is skipped, as before.
    let mut stores: HashMap<String, Box<dyn GraphStore>> = HashMap::new();
    for name in &reachable {
        if let Some(store) = prefetched.remove(name) {
            stores.insert(name.clone(), store);
            continue;
        }
        let Some(entry) = registry.get(name) else {
            continue;
        };
        if entry.health() != RepoHealth::Indexed {
            continue;
        }
        let ladybug = RepoPaths::new(entry.active_root())
            .index_dir
            .join("ladybug");
        match LadybugStore::open(&ladybug) {
            Ok(store) => {
                stores.insert(name.clone(), Box::new(store));
            }
            Err(e) => {
                eprintln!("note: skipping repo '{name}': cannot open its index ({e})");
            }
        }
    }
    let current_store = stores
        .get(&current)
        .ok_or_else(|| anyhow!("current repo '{current}' has no index — run `analyze` first"))?;

    // Resolve seeds locally in the current repo.
    let local_seeds: Vec<NodeId> = match seeds {
        SeedSpec::Name(name) => {
            let nodes = current_store.find_by_name(&name)?;
            if nodes.is_empty() {
                bail!("no symbol named '{name}' in the index");
            }
            nodes.into_iter().map(|n| n.id).collect()
        }
        SeedSpec::Change(spec) => {
            let files = changed_files(current_root, &spec)?;
            seed_nodes(current_store.as_ref(), &files)?.seeds
        }
    };

    // Origin packages each seed belongs to, for crate-level propagation (#237):
    // a crate-level consumer of one of these packages is flagged as potentially
    // affected even though no specific symbol resolved.
    let origin_packages: &[IndexedPackage] = identities
        .iter()
        .find(|r| r.repo == current)
        .map(|r| r.identity.packages.as_slice())
        .unwrap_or(&[]);
    let mut seed_packages: HashSet<String> = HashSet::new();
    for seed in &local_seeds {
        if let Some(node) = current_store.get_node(&seed.0)?
            && let Some(pkg) = owning_package(origin_packages, &node.file)
        {
            seed_packages.insert(pkg.to_string());
        }
    }

    // Build the qualified cross-edge table from symbol-level links: each
    // producer export -> the consumer's symbols in the importing file.
    let mut cross: HashMap<NodeId, Vec<(NodeId, Confidence)>> = HashMap::new();
    let mut crate_level: Vec<CrateLevelHit> = Vec::new();
    for link in &links {
        match &link.to_node {
            // Symbol-level link: add a cross-edge from the producer export to the
            // consumer use-sites. Precise use-sites (#236) land on exactly the
            // referencing symbols; otherwise fall back to every symbol in the
            // importing file.
            Some(node_id) => {
                let producer = qualify(&link.to_repo, &NodeId(node_id.clone()));
                let edges = cross.entry(producer).or_default();
                if !link.from_nodes.is_empty() {
                    for n in &link.from_nodes {
                        edges.push((
                            qualify(&link.from_repo, &NodeId(n.clone())),
                            Confidence::Inferred,
                        ));
                    }
                } else if let Some(consumer) = stores.get(&link.from_repo) {
                    for node in consumer.nodes_in_file(&link.from_file)? {
                        if is_symbol_node(node.kind) {
                            edges.push((qualify(&link.from_repo, &node.id), Confidence::Inferred));
                        }
                    }
                }
            }
            // Crate-level link (unresolved symbol): flag the consumer when it
            // depends on a producer package the seeds actually touch.
            None => {
                if link.to_repo == current && seed_packages.contains(&link.to_package) {
                    crate_level.push(CrateLevelHit {
                        repo: link.from_repo.clone(),
                        package: link.from_package.clone(),
                        file: link.from_file.clone(),
                        via: link.to_package.clone(),
                    });
                }
            }
        }
    }

    // Borrow each store as an Adjacency; the owned stores stay for node lookups.
    let repos_adj: HashMap<String, &dyn Adjacency> = stores
        .iter()
        .map(|(name, s)| (name.clone(), &**s as &dyn Adjacency))
        .collect();
    let fed = FederatedAdjacency::new(repos_adj, cross);
    let seeds: Vec<NodeId> = local_seeds.iter().map(|s| qualify(&current, s)).collect();

    // Classify impacted nodes, grouped by owning repo.
    let mut by_repo: BTreeMap<String, Vec<ClassifiedItem>> = BTreeMap::new();
    if !seeds.is_empty() {
        for it in compute_impact(&seeds, policy, &fed) {
            let (repo, local) = unqualify(&it.node);
            let Some(store) = stores.get(repo) else {
                continue;
            };
            if let Some(node) = store.get_node(local)? {
                by_repo
                    .entry(repo.to_string())
                    .or_default()
                    .push(ClassifiedItem {
                        id: node.id.0,
                        name: node.name,
                        qualified_name: node.qualified_name.clone(),
                        kind: node.kind,
                        file: node.file.clone(),
                        line: node.span.map(|sp| sp.start_line),
                        class: classify(node.kind, &node.file, &node.qualified_name),
                        distance: it.distance,
                        min_confidence: it.min_confidence,
                        cross_boundary: is_cross_boundary_path(&it.path),
                        path: it.path.iter().map(|k| k.as_str().to_string()).collect(),
                    });
            }
        }
    }

    let repos = by_repo
        .into_iter()
        .map(|(repo, items)| RepoImpact {
            origin: repo == current,
            repo,
            items,
        })
        .collect();
    Ok(FederatedReport::new(repos, crate_level))
}
