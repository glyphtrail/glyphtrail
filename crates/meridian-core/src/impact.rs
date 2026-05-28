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

use crate::{Confidence, EdgeKind, NodeId};

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
    /// In-process reverse dependencies: callers, importers, implementors,
    /// subtypes. The classic "what breaks if this symbol changes".
    pub fn in_process(max_depth: usize) -> Self {
        Self {
            edges: vec![
                EdgeRule::incoming(EdgeKind::Calls),
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
pub fn compute_impact<A: Adjacency>(
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
