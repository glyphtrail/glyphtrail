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
use glyphtrail_core::config::{Config, RepoPaths};
use glyphtrail_core::{
    Adjacency, ClassifiedItem, Confidence, CrateLevelHit, FederatedAdjacency, FederatedReport,
    Groups, HttpMethod, ImpactPolicy, IndexedPackage, META_EXTERNAL_USES, META_PACKAGES, NodeId,
    NodeKind, PackageIdentity, Protocol, Registry, RepoHealth, RepoIdentity, RepoImpact, classify,
    compute_impact, default_groups_path, default_registry_path, is_cross_boundary_path,
    path_signature, qualify, resolve_links, unqualify,
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

/// A manual cross-repo hint (#281) with each side's repo resolved to a concrete
/// registry name (`.`/absent already replaced by the declaring repo).
struct ResolvedHint {
    from_repo: String,
    from_symbol: Option<String>,
    from_endpoint: Option<String>,
    to_repo: String,
    to_symbol: Option<String>,
    to_endpoint: Option<String>,
}

/// Resolve a hint side's `repo` to a registry name for federation: the shared
/// resolver ([`glyphtrail_core::resolved_link_repo`]), falling back to the value
/// verbatim when it names no registered repo (best-effort; the link tooling
/// instead reports such a value as a dead link, #418).
fn resolve_link_repo(
    repo: &Option<String>,
    owner_name: &str,
    owner_root: &Path,
    registry: &Registry,
) -> String {
    glyphtrail_core::resolved_link_repo(repo, owner_name, owner_root, registry)
        .unwrap_or_else(|| repo.clone().unwrap_or_else(|| owner_name.to_string()))
}

/// The node ids one side of a precise hint refers to: a `symbol` matched by name
/// and/or an `endpoint` matched by REST operation signature (#407).
fn side_node_ids(
    store: &dyn GraphStore,
    symbol: &Option<String>,
    endpoint: &Option<String>,
) -> Result<Vec<NodeId>> {
    let mut ids = Vec::new();
    if let Some(sym) = symbol {
        ids.extend(store.find_by_name(sym)?.into_iter().map(|n| n.id));
    }
    if let Some(spec) = endpoint {
        ids.extend(endpoint_op_ids(store, spec)?);
    }
    // Dedup (a symbol and an endpoint can resolve to the same node) so a hint
    // doesn't emit repeated cross-edges.
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    Ok(ids)
}

/// Node ids of REST operations (`Endpoint`/`ClientCall`) in `store` whose
/// signature matches the endpoint spec (`"POST /signin"`, or `"/signin"` for any
/// method), so a hint can pin a call to an endpoint by route rather than symbol.
fn endpoint_op_ids(store: &dyn GraphStore, spec: &str) -> Result<Vec<NodeId>> {
    let Some((method, path_sig)) = parse_endpoint_spec(spec) else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    // Only code-side operations — a `SchemaOp` also carries a REST key but isn't
    // a call or endpoint to pin.
    for kind in [NodeKind::Endpoint, NodeKind::ClientCall] {
        ids.extend(
            store
                .operations_by_kind(kind)?
                .into_iter()
                .filter_map(|(id, key)| {
                    (key.protocol == Protocol::Rest
                        && method.is_none_or(|m| key.method == Some(m))
                        && path_signature(&key.path) == path_sig)
                        .then_some(id)
                }),
        );
    }
    Ok(ids)
}

/// Parse an endpoint spec into `(method?, path signature)`: `"POST /x"` →
/// `(Some(POST), sig("/x"))`; a bare `"/x"` → `(None, sig("/x"))` (any method).
fn parse_endpoint_spec(spec: &str) -> Option<(Option<HttpMethod>, String)> {
    let spec = spec.trim();
    match spec.split_once(char::is_whitespace) {
        Some((m, p)) => Some((Some(HttpMethod::parse(m.trim())?), path_signature(p.trim()))),
        None if spec.starts_with('/') => Some((None, path_signature(spec))),
        None => None,
    }
}

/// Repo-level cross edges (`producer repo`, `consumer repo`) + node-level cross
/// edges (`producer node`, `consumer node`) from web-API matching (#406).
type WebLinks = (Vec<(String, String)>, Vec<(NodeId, NodeId)>);
/// Endpoints grouped by route signature: `signature -> [(repo, node id)]`.
type EndpointsBySig = HashMap<String, Vec<(String, NodeId)>>;

/// Automatic cross-repo web-API links (#406): a REST client call in one in-scope
/// repo whose route signature (method + normalized path) matches an `Endpoint`
/// in another repo. Returns repo-level edges (producer endpoint repo → consumer
/// call repo, so impact flows from a changed endpoint to the calls that hit it)
/// and the node-level cross-edges (qualified endpoint id → qualified call id).
/// Opens each in-scope repo's index to read its operations; one that won't open
/// is skipped. Trivial routes (`GET /`, all-dynamic paths) are ignored so a
/// generic path doesn't link everything to everything.
fn web_links(names: &[String], registry: &Registry) -> WebLinks {
    let mut endpoints: EndpointsBySig = HashMap::new();
    let mut calls: Vec<(String, String, NodeId)> = Vec::new();
    for name in names {
        let Some(entry) = registry.get(name) else {
            continue;
        };
        if entry.health() != RepoHealth::Indexed {
            continue;
        }
        let ladybug = RepoPaths::new(entry.active_root())
            .index_dir
            .join("ladybug");
        let store = match LadybugStore::open(&ladybug) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("note: skipping web links for '{name}': cannot open its index ({e})");
                continue;
            }
        };
        // Read both operation kinds up front; if either errors, skip the whole
        // repo rather than contribute partial (misleading) web links.
        let (eps, ccs) = match (
            store.operations_by_kind(NodeKind::Endpoint),
            store.operations_by_kind(NodeKind::ClientCall),
        ) {
            (Ok(eps), Ok(ccs)) => (eps, ccs),
            _ => {
                eprintln!("note: skipping web links for '{name}': cannot read its operations");
                continue;
            }
        };
        for (id, key) in eps {
            let sig = key.signature();
            if key.protocol == Protocol::Rest && has_literal_segment(&sig) {
                endpoints.entry(sig).or_default().push((name.clone(), id));
            }
        }
        for (id, key) in ccs {
            let sig = key.signature();
            if key.protocol == Protocol::Rest && has_literal_segment(&sig) {
                calls.push((name.clone(), sig, id));
            }
        }
    }
    web_match(&endpoints, &calls)
}

/// Join client calls to endpoints by route signature across repos (the pure core
/// of [`web_links`]): a call in one repo and an endpoint in another sharing a
/// signature yield a producer-endpoint → consumer-call edge.
fn web_match(endpoints: &EndpointsBySig, calls: &[(String, String, NodeId)]) -> WebLinks {
    let mut repo_edges = Vec::new();
    let mut seen_repo = HashSet::new();
    let mut node_edges = Vec::new();
    for (call_repo, sig, call_id) in calls {
        for (ep_repo, ep_id) in endpoints.get(sig).into_iter().flatten() {
            if ep_repo != call_repo {
                // Many calls to the same producer collapse to one repo-level edge
                // (it only drives reachability); keep every node-level edge.
                if seen_repo.insert((ep_repo.clone(), call_repo.clone())) {
                    repo_edges.push((ep_repo.clone(), call_repo.clone()));
                }
                node_edges.push((qualify(ep_repo, ep_id), qualify(call_repo, call_id)));
            }
        }
    }
    (repo_edges, node_edges)
}

/// Whether a REST operation signature (`rest|METHOD|/seg/seg`) has a concrete
/// (non-parameter, non-empty) path segment, so a generic route like `GET /` or
/// `/{}` is not treated as matching every call.
fn has_literal_segment(signature: &str) -> bool {
    signature
        .rsplit('|')
        .next()
        .is_some_and(|path| path.split('/').any(|s| !s.is_empty() && s != "{}"))
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

    // Manual cross-repo hints (#281): each in-scope repo may declare links the
    // auto-resolver can't infer (an HTTP call with no shared package, say).
    // `from` (consumer) depends on `to` (producer) — changing `to` impacts
    // `from` — and a side's repo defaults to the declaring repo (".").
    let mut hints: Vec<ResolvedHint> = Vec::new();
    for name in &names {
        if let Some(entry) = registry.get(name) {
            // Links live in the repo's unified config (`glyphtrail.toml` +
            // personal override + legacy files), loaded best-effort.
            let owner_root = entry.active_root();
            let links = Config::load(owner_root)
                .map(|c| c.links)
                .unwrap_or_default();
            for h in links {
                hints.push(ResolvedHint {
                    from_repo: resolve_link_repo(&h.from.repo, name, owner_root, &registry),
                    from_symbol: h.from.symbol,
                    from_endpoint: h.from.endpoint,
                    to_repo: resolve_link_repo(&h.to.repo, name, owner_root, &registry),
                    to_symbol: h.to.symbol,
                    to_endpoint: h.to.endpoint,
                });
            }
        }
    }

    // Only repos reachable from the current repo along producer -> consumer edges
    // (a consumer's external use of a producer is a link `to_repo -> from_repo`)
    // can receive impact from its seeds, so only those need their store opened —
    // not the whole registry. BFS the repo-level link graph from the current repo;
    // manual hints contribute their `to_repo -> from_repo` edges too.
    // Automatic cross-repo web links (#406): match each in-scope repo's REST
    // client calls to endpoints in other repos by route signature, so a backend
    // route change reaches the frontends that call it without a shared package or
    // a manual hint. Opens the in-scope indexes; scoped by the query (a group
    // bounds it; the whole registry opens every member).
    let (web_repo_edges, web_node_edges) = web_links(&names, &registry);

    let mut consumers_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for l in &links {
        consumers_of
            .entry(l.to_repo.as_str())
            .or_default()
            .push(l.from_repo.as_str());
    }
    for h in &hints {
        consumers_of
            .entry(h.to_repo.as_str())
            .or_default()
            .push(h.from_repo.as_str());
    }
    for (producer, consumer) in &web_repo_edges {
        consumers_of
            .entry(producer.as_str())
            .or_default()
            .push(consumer.as_str());
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

    // Automatic web links (#406): a cross-edge from each matched backend endpoint
    // node to the frontend client-call node that hits it, so impact flows along
    // the HTTP boundary like a resolved package link.
    for (producer, consumer) in &web_node_edges {
        cross
            .entry(producer.clone())
            .or_default()
            .push((consumer.clone(), Confidence::Inferred));
    }

    // Manual hints (#281): a precise symbol↔symbol hint adds a cross-edge from the
    // producer (`to`) symbol to the consumer (`from`) symbol — so changing the
    // producer reaches the consumer, exactly like an auto-resolved link. A coarse
    // hint (a side without a symbol) flags the whole `from` repo when the producer
    // side is the repo being changed.
    for h in &hints {
        let to_precise = h.to_symbol.is_some() || h.to_endpoint.is_some();
        let from_precise = h.from_symbol.is_some() || h.from_endpoint.is_some();
        if to_precise && from_precise {
            // A precise hint: each side resolves to nodes by symbol name or, for
            // a call/endpoint, by REST operation signature (#407). Cross-edge from
            // each producer (`to`) node to each consumer (`from`) node.
            let (Some(to_store), Some(from_store)) =
                (stores.get(&h.to_repo), stores.get(&h.from_repo))
            else {
                continue; // a referenced repo isn't indexed/openable — skip
            };
            let consumers = side_node_ids(&**from_store, &h.from_symbol, &h.from_endpoint)?;
            let producers = side_node_ids(&**to_store, &h.to_symbol, &h.to_endpoint)?;
            for pid in &producers {
                let edges = cross.entry(qualify(&h.to_repo, pid)).or_default();
                for cid in &consumers {
                    edges.push((qualify(&h.from_repo, cid), Confidence::Inferred));
                }
            }
        } else if h.to_repo == current && !local_seeds.is_empty() {
            // Coarse whole-repo hint: relevant when the producer side is the repo
            // being changed (seeds live in `current`).
            crate_level.push(CrateLevelHit {
                repo: h.from_repo.clone(),
                package: h.from_repo.clone(),
                file: "(manual link hint)".to_string(),
                via: h.to_repo.clone(),
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn parse_endpoint_spec_handles_method_and_path() {
        // "METHOD /path" -> method + path signature.
        let (m, sig) = parse_endpoint_spec("POST /signin").unwrap();
        check!(m == Some(HttpMethod::Post));
        check!(sig == path_signature("/signin"));
        // A concrete param collapses in the signature, so a hint path with a
        // placeholder matches a route with a different concrete value.
        let (m, sig) = parse_endpoint_spec("GET /users/{id}").unwrap();
        check!(m == Some(HttpMethod::Get));
        check!(sig == path_signature("/users/42"));
        // A bare path -> any method.
        let (m, sig) = parse_endpoint_spec("/signin").unwrap();
        check!(m.is_none() && sig == path_signature("/signin"));
        // Garbage / unknown method -> None.
        check!(parse_endpoint_spec("nope").is_none());
        check!(parse_endpoint_spec("BOGUS /x").is_none());
    }

    #[test]
    fn endpoint_op_ids_matches_by_route_signature() {
        use glyphtrail_core::{Node, OperationKey};
        let dir = std::env::temp_dir().join(format!(
            "glyphtrail-fed-ep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut store = LadybugStore::open(&dir).unwrap();
        let mk = |id: &str, name: &str, kind: NodeKind| Node {
            id: NodeId(id.into()),
            kind,
            name: name.into(),
            qualified_name: name.into(),
            file: "r.ts".into(),
            language: None,
            span: None,
            doc: None,
            signature: None,
        };
        store
            .insert_graph(
                &[
                    mk("ep", "POST /signin", NodeKind::Endpoint),
                    mk("other", "GET /health", NodeKind::Endpoint),
                ],
                &[],
            )
            .unwrap();
        store
            .insert_operations(&[
                // A concrete path param; the hint uses a placeholder.
                (
                    NodeId("ep".into()),
                    OperationKey::rest(HttpMethod::Post, "/signin"),
                ),
                (
                    NodeId("other".into()),
                    OperationKey::rest(HttpMethod::Get, "/health"),
                ),
            ])
            .unwrap();

        let ids = endpoint_op_ids(&store, "POST /signin").unwrap();
        check!(ids == vec![NodeId("ep".into())]);
        // Method matters; a bare path matches any method.
        check!(endpoint_op_ids(&store, "GET /signin").unwrap().is_empty());
        check!(endpoint_op_ids(&store, "/signin").unwrap() == vec![NodeId("ep".into())]);
        // side_node_ids combines the endpoint resolution with a symbol lookup.
        let ids = side_node_ids(&store, &None, &Some("POST /signin".into())).unwrap();
        check!(ids == vec![NodeId("ep".into())]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_link_repo_maps_a_path_to_the_registered_name() {
        use glyphtrail_core::RegistryEntry;
        let base = std::env::temp_dir().join(format!(
            "gt-linkpath-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let meta = base.join("meta");
        let sub = meta.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let mut registry = Registry::default();
        registry.repos.push(RegistryEntry {
            name: "subrepo".into(),
            root: sub.clone(),
            alt_roots: vec![],
            missing_since: None,
            ids: vec![],
            contributors: vec![],
            identity: None,
            visibility: glyphtrail_core::Visibility::default(),
        });
        let r =
            |s: Option<&str>| resolve_link_repo(&s.map(String::from), "owner", &meta, &registry);
        check!(r(None) == "owner"); // "." / absent -> declaring repo
        check!(r(Some(".")) == "owner");
        check!(r(Some("backend")) == "backend"); // a name passes through
        check!(r(Some("./sub")) == "subrepo"); // a path -> the registered name
        check!(r(Some("./nope")) == "./nope"); // unresolvable path kept verbatim
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn has_literal_segment_rejects_generic_routes() {
        check!(has_literal_segment("rest|GET|/users/{}"));
        check!(has_literal_segment("rest|POST|/signin"));
        // All-dynamic / root routes don't anchor a match.
        check!(!has_literal_segment("rest|GET|/{}"));
        check!(!has_literal_segment("rest|GET|/"));
    }

    #[test]
    fn web_match_links_calls_to_endpoints_across_repos() {
        let id = |s: &str| NodeId(s.into());
        let endpoints = HashMap::from([
            (
                "rest|POST|/signin".to_string(),
                vec![("backend".to_string(), id("ep"))],
            ),
            (
                "rest|GET|/health".to_string(),
                vec![("backend".to_string(), id("hp"))],
            ),
        ]);
        let calls = vec![
            // web -> backend's /signin: a cross-repo match.
            (
                "web".to_string(),
                "rest|POST|/signin".to_string(),
                id("call"),
            ),
            // a call with no matching endpoint anywhere.
            ("web".to_string(), "rest|PUT|/other".to_string(), id("c2")),
            // a same-repo match must NOT produce a cross edge.
            (
                "backend".to_string(),
                "rest|GET|/health".to_string(),
                id("self"),
            ),
        ];
        let (repo_edges, node_edges) = web_match(&endpoints, &calls);
        check!(repo_edges == vec![("backend".to_string(), "web".to_string())]);
        check!(node_edges == vec![(qualify("backend", &id("ep")), qualify("web", &id("call")))]);
    }
}
