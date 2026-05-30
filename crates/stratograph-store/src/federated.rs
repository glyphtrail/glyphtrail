//! Cross-repo blast-radius orchestration (#222 / #223).
//!
//! Ties the registry, the persisted package identity, cross-repo link
//! resolution and the [`FederatedAdjacency`] together into one entry point,
//! [`federated_impact`], shared by the `stratograph impact --downstream` CLI and
//! the `impact` MCP tool. It opens every member repo's store, seeds in the
//! current repo, traverses across the package boundary, and returns a
//! [`FederatedReport`] (origin repo first, then downstream).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use stratograph_core::config::RepoPaths;
use stratograph_core::{
    Adjacency, ClassifiedItem, Confidence, FederatedAdjacency, FederatedReport, Groups,
    ImpactPolicy, META_EXTERNAL_USES, META_PACKAGES, NodeId, NodeKind, PackageIdentity, Registry,
    RepoHealth, RepoIdentity, RepoImpact, classify, compute_impact, default_groups_path,
    default_registry_path, is_cross_boundary_path, qualify, resolve_links, unqualify,
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

/// Compute the cross-repo blast radius: seed in the repo at `current_root` and
/// traverse into downstream repos across the link table, scoped to the whole
/// registry or a named group. The current repo must be registered and indexed.
pub fn federated_impact(
    current_root: &Path,
    scope: &FederationScope,
    seeds: SeedSpec,
    policy: &ImpactPolicy,
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
        .find(|e| e.root.canonicalize().map(|r| r == here).unwrap_or(false))
        .map(|e| e.name.clone())
        .ok_or_else(|| {
            anyhow!(
                "repo {} is not registered — `stratograph repo add` it to federate",
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

    // Open every indexed member, keyed by registry name.
    let mut stores: HashMap<String, Box<dyn GraphStore>> = HashMap::new();
    for name in &names {
        let Some(entry) = registry.get(name) else {
            continue;
        };
        if entry.health() != RepoHealth::Indexed {
            continue;
        }
        let ladybug = RepoPaths::new(&entry.root).index_dir.join("ladybug");
        stores.insert(name.clone(), Box::new(LadybugStore::open(&ladybug)?));
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

    // Build the qualified cross-edge table from symbol-level links: each
    // producer export -> the consumer's symbols in the importing file.
    let identities = stores
        .iter()
        .map(|(name, s)| {
            Ok(RepoIdentity {
                repo: name.clone(),
                identity: PackageIdentity::from_meta(
                    s.get_meta(META_PACKAGES)?.as_deref(),
                    s.get_meta(META_EXTERNAL_USES)?.as_deref(),
                ),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut cross: HashMap<NodeId, Vec<(NodeId, Confidence)>> = HashMap::new();
    for link in resolve_links(&identities) {
        let (Some(node_id), Some(consumer)) = (&link.to_node, stores.get(&link.from_repo)) else {
            continue; // crate-level links carry no producer node to seed from
        };
        let producer = qualify(&link.to_repo, &NodeId(node_id.clone()));
        for node in consumer.nodes_in_file(&link.from_file)? {
            if is_symbol_node(node.kind) {
                cross
                    .entry(producer.clone())
                    .or_default()
                    .push((qualify(&link.from_repo, &node.id), Confidence::Inferred));
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
    Ok(FederatedReport::new(repos))
}
