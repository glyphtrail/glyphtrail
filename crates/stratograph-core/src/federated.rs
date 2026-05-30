//! Federated graph adjacency for cross-repo impact (#222).
//!
//! The impact engine ([`crate::compute_impact`]) walks a single [`Adjacency`].
//! To compute a blast radius that crosses repository boundaries we wrap several
//! repos' adjacencies plus a table of cross-repo edges behind one `Adjacency`,
//! so the engine itself stays unchanged.
//!
//! **Node identity.** Within a repo, node ids are repo-local. The federation
//! layer qualifies them as `"<repo>\x1f<local>"` (the ASCII unit separator,
//! which never occurs in a repo name or a hex node id) so a step can be routed
//! to the owning repo and impacted nodes carry their repo. Seeds must be
//! qualified with [`qualify`] before traversal.
//!
//! **Cross-repo edges.** A symbol-level cross-repo link (a consumer importing a
//! producer's exported symbol) is modelled as an `Imports` edge traversed
//! `Incoming` — the same channel the in-process policy already walks for
//! reverse dependencies — so reaching a producer export yields its downstream
//! consumer nodes in other repos with no change to the engine or the default
//! policy. The cross-edge table is keyed by the qualified producer node and
//! built by the caller (it needs store access to resolve consumer symbols).

use std::collections::HashMap;

use crate::{Adjacency, Confidence, Direction, EdgeKind, NodeId};

/// Separator between the repo name and the repo-local id in a qualified node id.
const SEP: char = '\u{1f}';

/// Qualify a repo-local node id with its repo name for federated traversal.
pub fn qualify(repo: &str, local: &NodeId) -> NodeId {
    NodeId(format!("{repo}{SEP}{}", local.0))
}

/// Split a qualified node id back into `(repo, local)`. A id without the
/// separator is treated as repo-less (`("", id)`), which simply matches no
/// member store.
pub fn unqualify(id: &NodeId) -> (&str, &str) {
    match id.0.split_once(SEP) {
        Some((repo, local)) => (repo, local),
        None => ("", id.0.as_str()),
    }
}

/// An [`Adjacency`] over several repos plus a cross-repo edge table. Each step
/// is routed to the owning repo's adjacency and its neighbours re-qualified;
/// when a producer export node is reached over `Imports`/`Incoming`, its
/// downstream consumer nodes in other repos are added.
///
/// The per-repo adjacencies are *borrowed* (`&dyn Adjacency`), so the caller can
/// keep the underlying stores to look up node details for reporting while the
/// traversal runs.
pub struct FederatedAdjacency<'a> {
    repos: HashMap<String, &'a dyn Adjacency>,
    /// Qualified producer node -> qualified consumer nodes that depend on it
    /// across the package boundary, each with the cross-edge confidence.
    cross: HashMap<NodeId, Vec<(NodeId, Confidence)>>,
}

impl<'a> FederatedAdjacency<'a> {
    /// Build from per-repo adjacencies and a prebuilt cross-edge table. Both the
    /// `cross` keys and its target node ids must already be qualified.
    pub fn new(
        repos: HashMap<String, &'a dyn Adjacency>,
        cross: HashMap<NodeId, Vec<(NodeId, Confidence)>>,
    ) -> Self {
        Self { repos, cross }
    }

    /// The repos spanned, for reporting.
    pub fn repos(&self) -> impl Iterator<Item = &str> {
        self.repos.keys().map(String::as_str)
    }
}

impl Adjacency for FederatedAdjacency<'_> {
    fn step(&self, node: &NodeId, kind: EdgeKind, dir: Direction) -> Vec<(NodeId, Confidence)> {
        let (repo, local) = unqualify(node);
        let mut out = Vec::new();
        if let Some(adj) = self.repos.get(repo) {
            let local = NodeId(local.to_string());
            for (neighbour, conf) in adj.step(&local, kind, dir) {
                out.push((qualify(repo, &neighbour), conf));
            }
        }
        // Cross-repo edges ride the Imports/Incoming channel (see module docs).
        if kind == EdgeKind::Imports
            && dir == Direction::Incoming
            && let Some(consumers) = self.cross.get(node)
        {
            out.extend(consumers.iter().cloned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImpactPolicy, compute_impact};
    use assert2::check;
    use std::collections::HashMap;

    /// Minimal in-memory adjacency: incoming edges keyed by (dst, kind).
    #[derive(Default)]
    struct Mock {
        inc: HashMap<(String, EdgeKind), Vec<(NodeId, Confidence)>>,
    }
    impl Mock {
        fn edge(&mut self, src: &str, dst: &str, kind: EdgeKind) {
            self.inc
                .entry((dst.into(), kind))
                .or_default()
                .push((NodeId(src.into()), Confidence::Extracted));
        }
    }
    impl Adjacency for Mock {
        fn step(&self, node: &NodeId, kind: EdgeKind, dir: Direction) -> Vec<(NodeId, Confidence)> {
            match dir {
                Direction::Incoming => self
                    .inc
                    .get(&(node.0.clone(), kind))
                    .cloned()
                    .unwrap_or_default(),
                Direction::Outgoing => Vec::new(),
            }
        }
    }

    #[test]
    fn qualify_round_trips() {
        let q = qualify("repoA", &NodeId("abc123".into()));
        check!(unqualify(&q) == ("repoA", "abc123"));
    }

    #[test]
    fn impact_crosses_the_repo_boundary() {
        // Producer repo `p`: caller `pc` --Calls--> exported `pe`.
        // Consumer repo `c`: `cc` --Calls--> `cuse` (the importing symbol).
        // Cross edge: producer export `pe` -> consumer `cuse`.
        // Changing `pe` should impact `pc` (same repo) AND `cuse` + `cc` (across).
        let mut p = Mock::default();
        p.edge("pc", "pe", EdgeKind::Calls);
        let mut c = Mock::default();
        c.edge("cc", "cuse", EdgeKind::Calls);

        let mut repos: HashMap<String, &dyn Adjacency> = HashMap::new();
        repos.insert("p".into(), &p as &dyn Adjacency);
        repos.insert("c".into(), &c as &dyn Adjacency);

        let mut cross = HashMap::new();
        cross.insert(
            qualify("p", &NodeId("pe".into())),
            vec![(qualify("c", &NodeId("cuse".into())), Confidence::Inferred)],
        );

        let fed = FederatedAdjacency::new(repos, cross);
        let seed = qualify("p", &NodeId("pe".into()));
        let items = compute_impact(&[seed], &ImpactPolicy::in_process(5), &fed);
        let ids: Vec<&str> = items.iter().map(|i| i.node.0.as_str()).collect();

        check!(ids.contains(&qualify("p", &NodeId("pc".into())).0.as_str()));
        check!(ids.contains(&qualify("c", &NodeId("cuse".into())).0.as_str()));
        check!(ids.contains(&qualify("c", &NodeId("cc".into())).0.as_str()));
        // The cross hop is inferred, so anything beyond it is at most inferred.
        let cuse = items
            .iter()
            .find(|i| i.node == qualify("c", &NodeId("cuse".into())))
            .unwrap();
        check!(cuse.min_confidence == Confidence::Inferred);
    }
}
