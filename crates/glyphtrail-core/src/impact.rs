//! Generalized impact-traversal engine: the blast radius of a change.
//!
//! Given one or more **seed** nodes (the thing that changed), walk
//! reverse-dependency edges of several kinds to find every node that could be
//! affected, recording for each impacted node *how far* it is, the *minimum
//! confidence* along the path that reached it, and an example *edge-kind path*
//! back toward a seed.
//!
//! The algorithm lives here (pure, unit-testable against an in-memory graph);
//! the real graph is supplied through the [`Adjacency`] trait, implemented by
//! the storage backend.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;

use crate::{Confidence, EdgeKind, NodeId, NodeKind};

/// Which way to walk an edge kind during traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow edges *into* the current node (`dst = current`); the source is the
    /// neighbour. This is the reverse-dependency direction for `Calls`,
    /// `Imports`, `Implements`, `Extends`, `Invokes`, `Mounts`.
    Incoming,
    /// Follow edges *out of* the current node (`src = current`); the
    /// destination is the neighbour. Used for `Handles` (handler → endpoint)
    /// and `Exposes` (endpoint → schema op).
    Outgoing,
}

/// One edge kind plus the direction to traverse it for impact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeRule {
    pub kind: EdgeKind,
    pub dir: Direction,
}

impl EdgeRule {
    pub fn incoming(kind: EdgeKind) -> Self {
        Self {
            kind,
            dir: Direction::Incoming,
        }
    }
    pub fn outgoing(kind: EdgeKind) -> Self {
        Self {
            kind,
            dir: Direction::Outgoing,
        }
    }
}

/// Map user edge-set tokens (`calls`, `imports`, `impl`, `api`) to rules.
/// Shared by the CLI `impact` command and the MCP `impact` tool.
pub fn edge_rules(tokens: &[&str]) -> Result<Vec<EdgeRule>, String> {
    let mut rules = Vec::new();
    for t in tokens {
        match *t {
            "calls" => rules.push(EdgeRule::incoming(EdgeKind::Calls)),
            "refs" => rules.push(EdgeRule::incoming(EdgeKind::References)),
            "imports" => rules.push(EdgeRule::incoming(EdgeKind::Imports)),
            "impl" => {
                rules.push(EdgeRule::incoming(EdgeKind::Implements));
                rules.push(EdgeRule::incoming(EdgeKind::Extends));
            }
            "api" => {
                rules.push(EdgeRule::outgoing(EdgeKind::Handles));
                rules.push(EdgeRule::outgoing(EdgeKind::Exposes));
                rules.push(EdgeRule::incoming(EdgeKind::Invokes));
                rules.push(EdgeRule::incoming(EdgeKind::Mounts));
            }
            other => {
                return Err(format!(
                    "unknown edge set '{other}' (expected calls, refs, imports, impl, api)"
                ));
            }
        }
    }
    Ok(rules)
}

/// Parse a confidence token (`extracted` | `inferred`).
pub fn parse_confidence(s: &str) -> Result<Confidence, String> {
    match s.to_ascii_lowercase().as_str() {
        "extracted" => Ok(Confidence::Extracted),
        "inferred" => Ok(Confidence::Inferred),
        other => Err(format!(
            "unknown confidence '{other}' (expected extracted or inferred)"
        )),
    }
}

/// Traversal configuration.
#[derive(Debug, Clone)]
pub struct ImpactPolicy {
    /// Edge kinds + directions to traverse, in the order they are tried.
    pub edges: Vec<EdgeRule>,
    /// Maximum number of hops from a seed.
    pub max_depth: usize,
    /// Drop any node whose path min-confidence is weaker than this. With
    /// `Inferred` every reachable node is kept; with `Extracted` only
    /// fully-extracted paths survive.
    pub min_confidence: Confidence,
}

impl ImpactPolicy {
    /// In-process reverse dependencies: callers, type users, importers,
    /// implementors, subtypes. The classic "what breaks if this symbol changes".
    pub fn in_process(max_depth: usize) -> Self {
        Self {
            edges: vec![
                EdgeRule::incoming(EdgeKind::Calls),
                EdgeRule::incoming(EdgeKind::References),
                EdgeRule::incoming(EdgeKind::Imports),
                EdgeRule::incoming(EdgeKind::Implements),
                EdgeRule::incoming(EdgeKind::Extends),
            ],
            max_depth,
            min_confidence: Confidence::Inferred,
        }
    }

    /// Cross-boundary edges added on top of the in-process set: a changed
    /// handler reaches the endpoint it `Handles`, the schema op the endpoint
    /// `Exposes`, the clients that `Invokes` it, and parent routers via
    /// `Mounts`.
    pub fn cross_boundary(max_depth: usize) -> Self {
        let mut p = Self::in_process(max_depth);
        p.edges.extend([
            EdgeRule::outgoing(EdgeKind::Handles),
            EdgeRule::outgoing(EdgeKind::Exposes),
            EdgeRule::incoming(EdgeKind::Invokes),
            EdgeRule::incoming(EdgeKind::Mounts),
        ]);
        p
    }
}

/// An impacted node and how it was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactItem {
    pub node: NodeId,
    /// Hops from the nearest seed.
    pub distance: usize,
    /// Weakest edge confidence along the recorded path.
    pub min_confidence: Confidence,
    /// Edge kinds from a seed to this node (the recorded shortest/strongest path).
    pub path: Vec<EdgeKind>,
}

/// Actionable category for an impacted node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactClass {
    /// A test — the highest-value output ("run these").
    Test,
    /// Public API surface: a server endpoint or a declared schema operation.
    Api,
    /// A program entrypoint (e.g. `main`).
    Entrypoint,
    /// Everything else (internal dependents and client call sites — the latter
    /// are surfaced separately via the cross-boundary flag).
    Internal,
}

/// Classify an impacted node by kind, file path and qualified name, using
/// path/name heuristics for tests and entrypoints. Precedence:
/// test > api > entrypoint > internal. Client call sites are *not* API surface
/// — they are downstream consumers, flagged via `cross_boundary` instead.
pub fn classify(kind: NodeKind, file: &str, qualified_name: &str) -> ImpactClass {
    if is_test(file, qualified_name) {
        ImpactClass::Test
    } else if matches!(kind, NodeKind::Endpoint | NodeKind::SchemaOp) {
        ImpactClass::Api
    } else if is_entrypoint(kind, qualified_name) {
        ImpactClass::Entrypoint
    } else {
        ImpactClass::Internal
    }
}

fn is_test(file: &str, qualified_name: &str) -> bool {
    let f = file.replace('\\', "/").to_ascii_lowercase();
    let base = f.rsplit('/').next().unwrap_or(&f);
    f.contains("/tests/")
        || f.starts_with("tests/")
        || f.contains("/test/")
        || f.starts_with("test/")
        || base.contains(".test.")
        || base.contains(".spec.")
        || base.ends_with("_test.go")
        || base.starts_with("test_")
        || base.ends_with("_test.py")
        || base.ends_with("_test.rs")
        || qualified_name.contains("tests::")
}

fn is_entrypoint(kind: NodeKind, qualified_name: &str) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Method)
        && (qualified_name == "main" || qualified_name.ends_with("::main"))
}

/// Supplies graph adjacency to the engine. Implemented by the store.
pub trait Adjacency {
    /// Neighbours of `node` along `kind` in `dir`, each with the edge's
    /// confidence. Order need not be stable; the engine sorts internally.
    fn step(&self, node: &NodeId, kind: EdgeKind, dir: Direction) -> Vec<(NodeId, Confidence)>;
}

#[derive(Clone)]
struct Reach {
    distance: usize,
    min_confidence: Confidence,
    path: Vec<EdgeKind>,
}

/// Compute the impacted set from `seeds` under `policy`, using `adj` for graph
/// access. Seeds themselves are excluded from the result. Each impacted node is
/// kept with its shortest path; ties on distance prefer the stronger
/// min-confidence. Deterministic and cycle-safe.
pub fn compute_impact<A: Adjacency + ?Sized>(
    seeds: &[NodeId],
    policy: &ImpactPolicy,
    adj: &A,
) -> Vec<ImpactItem> {
    let seed_set: HashSet<&NodeId> = seeds.iter().collect();
    let mut best: HashMap<NodeId, Reach> = HashMap::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();

    // Seeds enter at distance 0 with maximal confidence and an empty path. They
    // drive expansion but are not themselves reported.
    let mut sorted_seeds: Vec<&NodeId> = seeds.iter().collect();
    sorted_seeds.sort_by(|a, b| a.0.cmp(&b.0));
    sorted_seeds.dedup();
    for s in sorted_seeds {
        best.insert(
            s.clone(),
            Reach {
                distance: 0,
                min_confidence: Confidence::Extracted,
                path: Vec::new(),
            },
        );
        queue.push_back(s.clone());
    }

    while let Some(cur) = queue.pop_front() {
        let Reach {
            distance,
            min_confidence,
            path,
        } = best[&cur].clone();
        if distance >= policy.max_depth {
            continue;
        }
        // Collect every outgoing step, then sort for deterministic processing.
        let mut steps: Vec<(NodeId, EdgeKind, Confidence)> = Vec::new();
        for rule in &policy.edges {
            for (m, c) in adj.step(&cur, rule.kind, rule.dir) {
                steps.push((m, rule.kind, c));
            }
        }
        steps.sort_by(|a, b| a.0.0.cmp(&b.0.0).then(a.1.as_str().cmp(b.1.as_str())));

        for (m, ek, edge_conf) in steps {
            if seed_set.contains(&m) {
                continue; // never report a seed as its own impact
            }
            let cand_conf = min_confidence.weaker(edge_conf);
            if cand_conf.rank() < policy.min_confidence.rank() {
                continue; // path too weak for this policy
            }
            let cand_dist = distance + 1;
            let better = match best.get(&m) {
                None => true,
                Some(prev) => {
                    cand_dist < prev.distance
                        || (cand_dist == prev.distance
                            && cand_conf.rank() > prev.min_confidence.rank())
                }
            };
            if better {
                let mut cand_path = path.clone();
                cand_path.push(ek);
                best.insert(
                    m.clone(),
                    Reach {
                        distance: cand_dist,
                        min_confidence: cand_conf,
                        path: cand_path,
                    },
                );
                queue.push_back(m);
            }
        }
    }

    let mut items: Vec<ImpactItem> = best
        .into_iter()
        .filter(|(id, _)| !seed_set.contains(id))
        .map(|(node, r)| ImpactItem {
            node,
            distance: r.distance,
            min_confidence: r.min_confidence,
            path: r.path,
        })
        .collect();
    // Rank: closest first, then stronger confidence, then id for stability.
    items.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then(b.min_confidence.rank().cmp(&a.min_confidence.rank()))
            .then(a.node.0.cmp(&b.node.0))
    });
    items
}

/// Whether a path reaches a downstream consumer *across the wire* — i.e. it
/// traverses an `Invokes` edge (a client calling an affected endpoint). Other
/// API edges (`Handles`/`Exposes`/`Mounts`) stay within the service and are
/// reported as ordinary API surface, not cross-boundary consumers.
pub fn is_cross_boundary_path(path: &[EdgeKind]) -> bool {
    path.contains(&EdgeKind::Invokes)
}

/// A classified, resolved impacted node, ready for reporting.
#[derive(Debug, Clone, Serialize)]
pub struct ClassifiedItem {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: NodeKind,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub class: ImpactClass,
    pub distance: usize,
    pub min_confidence: Confidence,
    /// Reached across the service boundary (HANDLES/INVOKES/EXPOSES/MOUNTS).
    pub cross_boundary: bool,
    /// Edge-kind path from a seed (string form for stable JSON).
    pub path: Vec<String>,
}

/// Blast-radius counts.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactSummary {
    pub total: usize,
    pub tests: usize,
    pub api: usize,
    pub entrypoints: usize,
    pub internal: usize,
    pub cross_boundary: usize,
    pub max_distance: usize,
}

/// The full impact report: summary + ranked, classified items + change notes.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactReport {
    pub summary: ImpactSummary,
    pub items: Vec<ClassifiedItem>,
    /// Files whose deletion removed symbols (former dependents can't be seeded).
    pub removed_files: Vec<String>,
    /// Changed files with no overlapping indexed symbol.
    pub unresolved_files: Vec<String>,
}

impl ImpactReport {
    /// Assemble a report from already-ranked, classified items. The summary is
    /// derived from the items.
    pub fn new(
        items: Vec<ClassifiedItem>,
        removed_files: Vec<String>,
        unresolved_files: Vec<String>,
    ) -> Self {
        let count = |c: ImpactClass| items.iter().filter(|i| i.class == c).count();
        let summary = ImpactSummary {
            total: items.len(),
            tests: count(ImpactClass::Test),
            api: count(ImpactClass::Api),
            entrypoints: count(ImpactClass::Entrypoint),
            internal: count(ImpactClass::Internal),
            cross_boundary: items.iter().filter(|i| i.cross_boundary).count(),
            max_distance: items.iter().map(|i| i.distance).max().unwrap_or(0),
        };
        ImpactReport {
            summary,
            items,
            removed_files,
            unresolved_files,
        }
    }

    /// One-line blast-radius headline.
    pub fn headline(&self) -> String {
        let s = &self.summary;
        format!(
            "blast radius: {} symbols, {} tests, {} API, {} cross-boundary consumers",
            s.total, s.tests, s.api, s.cross_boundary
        )
    }

    /// True if the change reaches the public API surface (an endpoint or a
    /// declared schema operation) — the signal for the CI drift gate.
    pub fn touches_contract(&self) -> bool {
        self.items.iter().any(|i| i.class == ImpactClass::Api)
    }

    /// Render a PR-comment / job-summary Markdown report.
    pub fn to_markdown(&self) -> String {
        let s = &self.summary;
        let mut md = String::new();
        md.push_str("## Impact analysis\n\n");
        md.push_str(&format!(
            "**Blast radius:** {} symbols · {} tests · {} API surface · {} cross-boundary consumers · max distance {}\n",
            s.total, s.tests, s.api, s.cross_boundary, s.max_distance
        ));

        let section = |md: &mut String, title: &str, rows: Vec<&ClassifiedItem>| {
            if rows.is_empty() {
                return;
            }
            md.push_str(&format!("\n### {title}\n\n"));
            for i in rows {
                let loc = match i.line {
                    Some(l) => format!("{}:{}", i.file, l),
                    None => i.file.clone(),
                };
                let conf = if i.min_confidence == Confidence::Inferred {
                    " _(inferred)_"
                } else {
                    ""
                };
                md.push_str(&format!(
                    "- `{}` ({}) — d{}{}\n",
                    i.qualified_name, loc, i.distance, conf
                ));
            }
        };

        section(
            &mut md,
            "Tests to run",
            self.items
                .iter()
                .filter(|i| i.class == ImpactClass::Test)
                .collect(),
        );
        section(
            &mut md,
            "API surface affected",
            self.items
                .iter()
                .filter(|i| i.class == ImpactClass::Api)
                .collect(),
        );
        section(
            &mut md,
            "Cross-boundary consumers",
            self.items.iter().filter(|i| i.cross_boundary).collect(),
        );
        if !self.removed_files.is_empty() {
            md.push_str("\n### Removed files (former dependents may be affected)\n\n");
            for f in &self.removed_files {
                md.push_str(&format!("- `{f}`\n"));
            }
        }
        md
    }
}

/// The impacted nodes in one repo, for a federated (cross-repo) report (#222).
#[derive(Debug, Clone, Serialize)]
pub struct RepoImpact {
    pub repo: String,
    /// True for the repo the change originates in; the rest are downstream.
    pub origin: bool,
    pub items: Vec<ClassifiedItem>,
}

/// A consumer crate-level-linked to an impacted producer package, where the
/// exact symbol couldn't be resolved (a `pub use` re-export, macro-generated
/// item, or parser gap). Coarser than a per-symbol item: it flags "this repo
/// depends on the changed crate, check it" rather than naming impacted nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrateLevelHit {
    /// Consumer repo that may be affected.
    pub repo: String,
    /// Consumer package declaring the dependency.
    pub package: String,
    /// Consumer file importing the producer crate.
    pub file: String,
    /// Impacted producer package the consumer depends on.
    pub via: String,
}

/// Cross-repo blast radius: an aggregate summary, per-repo impacted nodes
/// (origin first then downstream), and coarse crate-level "potentially
/// affected" consumers. The summary is derived from all per-node items.
#[derive(Debug, Clone, Serialize)]
pub struct FederatedReport {
    pub summary: ImpactSummary,
    pub repos: Vec<RepoImpact>,
    /// Consumers linked only at crate level to an impacted producer package
    /// (see [`CrateLevelHit`]); not counted in `summary`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crate_level: Vec<CrateLevelHit>,
}

impl FederatedReport {
    /// Assemble from per-repo impacts plus crate-level hits: order repos origin
    /// first then downstream alphabetically, sort/dedup the crate-level hits,
    /// and derive the aggregate summary across all per-node items.
    pub fn new(mut repos: Vec<RepoImpact>, mut crate_level: Vec<CrateLevelHit>) -> Self {
        repos.sort_by(|a, b| b.origin.cmp(&a.origin).then(a.repo.cmp(&b.repo)));
        crate_level.sort_by(|a, b| (&a.repo, &a.file, &a.via).cmp(&(&b.repo, &b.file, &b.via)));
        crate_level.dedup();
        let mut summary = ImpactSummary {
            total: 0,
            tests: 0,
            api: 0,
            entrypoints: 0,
            internal: 0,
            cross_boundary: 0,
            max_distance: 0,
        };
        for i in repos.iter().flat_map(|r| &r.items) {
            summary.total += 1;
            match i.class {
                ImpactClass::Test => summary.tests += 1,
                ImpactClass::Api => summary.api += 1,
                ImpactClass::Entrypoint => summary.entrypoints += 1,
                ImpactClass::Internal => summary.internal += 1,
            }
            if i.cross_boundary {
                summary.cross_boundary += 1;
            }
            summary.max_distance = summary.max_distance.max(i.distance);
        }
        FederatedReport {
            summary,
            repos,
            crate_level,
        }
    }

    /// Count of downstream (non-origin) repos with impacted nodes.
    pub fn downstream_repos(&self) -> usize {
        self.repos.iter().filter(|r| !r.origin).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use std::collections::HashMap;

    /// In-memory adjacency: edges keyed by (src, kind) -> [(dst, confidence)].
    #[derive(Default)]
    struct MockGraph {
        out: HashMap<(String, EdgeKind), Vec<(NodeId, Confidence)>>,
        inc: HashMap<(String, EdgeKind), Vec<(NodeId, Confidence)>>,
    }

    impl MockGraph {
        fn edge(&mut self, src: &str, dst: &str, kind: EdgeKind, c: Confidence) {
            self.out
                .entry((src.into(), kind))
                .or_default()
                .push((NodeId(dst.into()), c));
            self.inc
                .entry((dst.into(), kind))
                .or_default()
                .push((NodeId(src.into()), c));
        }
    }

    impl Adjacency for MockGraph {
        fn step(&self, node: &NodeId, kind: EdgeKind, dir: Direction) -> Vec<(NodeId, Confidence)> {
            let map = match dir {
                Direction::Incoming => &self.inc,
                Direction::Outgoing => &self.out,
            };
            map.get(&(node.0.clone(), kind))
                .cloned()
                .unwrap_or_default()
        }
    }

    fn id(s: &str) -> NodeId {
        NodeId(s.into())
    }

    #[test]
    fn callers_are_reached_transitively() {
        // c calls b calls a. Changing a impacts b (1) and c (2).
        let mut g = MockGraph::default();
        g.edge("b", "a", EdgeKind::Calls, Confidence::Extracted);
        g.edge("c", "b", EdgeKind::Calls, Confidence::Extracted);

        let items = compute_impact(&[id("a")], &ImpactPolicy::in_process(5), &g);
        let names: Vec<(&str, usize)> = items
            .iter()
            .map(|i| (i.node.0.as_str(), i.distance))
            .collect();
        check!(names == vec![("b", 1), ("c", 2)]);
    }

    // #310: a type used (not called) — `proto: Protocol` — is a References edge
    // from the user to the type, so the default in-process policy reaches a
    // type's users, transitively, the same way it reaches callers.
    #[test]
    fn type_references_reach_users() {
        let mut g = MockGraph::default();
        g.edge(
            "user",
            "Protocol",
            EdgeKind::References,
            Confidence::Extracted,
        );
        g.edge(
            "caller_of_user",
            "user",
            EdgeKind::Calls,
            Confidence::Extracted,
        );

        let items = compute_impact(&[id("Protocol")], &ImpactPolicy::in_process(5), &g);
        let names: Vec<&str> = items.iter().map(|i| i.node.0.as_str()).collect();
        check!(names.contains(&"user"));
        check!(names.contains(&"caller_of_user"));
    }

    #[test]
    fn refs_edge_set_traverses_references_incoming() {
        let rules = edge_rules(&["refs"]).unwrap();
        check!(rules == vec![EdgeRule::incoming(EdgeKind::References)]);
    }

    #[test]
    fn depth_limits_traversal() {
        let mut g = MockGraph::default();
        g.edge("b", "a", EdgeKind::Calls, Confidence::Extracted);
        g.edge("c", "b", EdgeKind::Calls, Confidence::Extracted);

        let items = compute_impact(&[id("a")], &ImpactPolicy::in_process(1), &g);
        check!(items.len() == 1);
        check!(items[0].node.0 == "b");
    }

    #[test]
    fn min_confidence_is_the_weakest_edge_on_the_path() {
        let mut g = MockGraph::default();
        g.edge("b", "a", EdgeKind::Calls, Confidence::Extracted);
        g.edge("c", "b", EdgeKind::Calls, Confidence::Inferred);

        let items = compute_impact(&[id("a")], &ImpactPolicy::in_process(5), &g);
        let by: HashMap<&str, &ImpactItem> = items.iter().map(|i| (i.node.0.as_str(), i)).collect();
        check!(by["b"].min_confidence == Confidence::Extracted);
        check!(by["c"].min_confidence == Confidence::Inferred); // weakened by b->c
    }

    #[test]
    fn min_confidence_filter_prunes_inferred_paths() {
        let mut g = MockGraph::default();
        g.edge("b", "a", EdgeKind::Calls, Confidence::Inferred);

        let mut policy = ImpactPolicy::in_process(5);
        policy.min_confidence = Confidence::Extracted;
        let items = compute_impact(&[id("a")], &policy, &g);
        check!(items.is_empty()); // the only path is inferred
    }

    #[test]
    fn cycles_are_safe() {
        let mut g = MockGraph::default();
        g.edge("b", "a", EdgeKind::Calls, Confidence::Extracted);
        g.edge("a", "b", EdgeKind::Calls, Confidence::Extracted); // cycle
        let items = compute_impact(&[id("a")], &ImpactPolicy::in_process(5), &g);
        // b is impacted (caller of a); a is a seed, not reported.
        check!(items.len() == 1);
        check!(items[0].node.0 == "b");
    }

    #[test]
    fn multi_seed_keeps_shortest_distance() {
        // d calls c calls a; d also calls b. Seeding {a, b}: c is dist 1 (via a),
        // d is dist 1 (via b) not 2 (via c).
        let mut g = MockGraph::default();
        g.edge("c", "a", EdgeKind::Calls, Confidence::Extracted);
        g.edge("d", "c", EdgeKind::Calls, Confidence::Extracted);
        g.edge("d", "b", EdgeKind::Calls, Confidence::Extracted);

        let items = compute_impact(&[id("a"), id("b")], &ImpactPolicy::in_process(5), &g);
        let by: HashMap<&str, usize> = items
            .iter()
            .map(|i| (i.node.0.as_str(), i.distance))
            .collect();
        check!(by["c"] == 1);
        check!(by["d"] == 1);
    }

    #[test]
    fn report_summary_markdown_and_gate() {
        let item = |kind: NodeKind, file: &str, name: &str, cb: bool| ClassifiedItem {
            id: name.into(),
            name: name.into(),
            qualified_name: name.into(),
            kind,
            file: file.into(),
            line: Some(1),
            class: classify(kind, file, name),
            distance: 1,
            min_confidence: Confidence::Extracted,
            cross_boundary: cb,
            path: vec!["calls".into()],
        };
        let report = ImpactReport::new(
            vec![
                item(NodeKind::Endpoint, "src/routes.rs", "get_user", false),
                item(NodeKind::Function, "tests/it.rs", "test_x", false),
            ],
            vec![],
            vec![],
        );
        check!(report.summary.total == 2);
        check!(report.summary.api == 1);
        check!(report.summary.tests == 1);
        check!(report.touches_contract()); // endpoint present -> gate trips
        let md = report.to_markdown();
        check!(md.contains("## Impact analysis"));
        check!(md.contains("Tests to run"));
        check!(md.contains("API surface affected"));
    }

    #[test]
    fn classification_heuristics() {
        check!(classify(NodeKind::Function, "src/tests/foo.rs", "foo") == ImpactClass::Test);
        check!(classify(NodeKind::Function, "web/api.test.ts", "loadUser") == ImpactClass::Test);
        check!(classify(NodeKind::Function, "svc/user_test.go", "TestX") == ImpactClass::Test);
        check!(classify(NodeKind::Endpoint, "src/routes.rs", "get_user") == ImpactClass::Api);
        check!(classify(NodeKind::Function, "src/main.rs", "main") == ImpactClass::Entrypoint);
        check!(classify(NodeKind::Function, "src/util.rs", "helper") == ImpactClass::Internal);
    }

    #[test]
    fn cross_boundary_path_detection() {
        check!(is_cross_boundary_path(&[EdgeKind::Calls]) == false);
        check!(is_cross_boundary_path(&[EdgeKind::Handles]) == false); // within-service
        check!(is_cross_boundary_path(&[
            EdgeKind::Handles,
            EdgeKind::Invokes
        ])); // across the wire
    }

    #[test]
    fn cross_boundary_reaches_clients_and_schema() {
        // handler --Handles--> endpoint; client --Invokes--> endpoint;
        // endpoint --Exposes--> schemaop. Changing the handler impacts all three.
        let mut g = MockGraph::default();
        g.edge(
            "handler",
            "endpoint",
            EdgeKind::Handles,
            Confidence::Extracted,
        );
        g.edge(
            "client",
            "endpoint",
            EdgeKind::Invokes,
            Confidence::Inferred,
        );
        g.edge(
            "endpoint",
            "schemaop",
            EdgeKind::Exposes,
            Confidence::Extracted,
        );

        let items = compute_impact(&[id("handler")], &ImpactPolicy::cross_boundary(5), &g);
        let by: HashMap<&str, &ImpactItem> = items.iter().map(|i| (i.node.0.as_str(), i)).collect();
        check!(by.contains_key("endpoint"));
        check!(by.contains_key("schemaop"));
        check!(by.contains_key("client"));
        // client reached across an inferred INVOKES edge -> min confidence inferred.
        check!(by["client"].min_confidence == Confidence::Inferred);
    }
}
