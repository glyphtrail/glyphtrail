//! Cross-boundary matching: link a client call to the endpoint that answers it.
//!
//! Each endpoint is reduced (via the [`RewriteEngine`]) to the set of external
//! [`OperationKey`] signatures a client could use to reach it, each carrying a
//! [`Confidence`]. A client call resolves to every endpoint whose signature it
//! matches; when an exact (`Extracted`) match exists it wins over heuristic
//! (`Inferred`) ones, while genuine same-confidence ambiguity is preserved.

use std::collections::HashMap;

use crate::api::OperationKey;
use crate::model::{Confidence, NodeId};
use crate::rewrite::RewriteEngine;

/// A server-side operation at an internal path, identified by its graph node.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: NodeId,
    pub key: OperationKey,
}

/// A client call site targeting an external operation, identified by its node.
#[derive(Debug, Clone)]
pub struct ClientCall {
    pub id: NodeId,
    pub key: OperationKey,
}

/// A resolved client→endpoint link (becomes an `INVOKES` edge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub client: NodeId,
    pub endpoint: NodeId,
    pub confidence: Confidence,
}

/// An index from operation signature to the endpoints reachable under it.
pub struct Matcher {
    index: HashMap<String, Vec<(NodeId, Confidence)>>,
}

impl Matcher {
    /// Build the match index by expanding every endpoint through the rewrite
    /// engine into its external candidate signatures.
    pub fn build(endpoints: &[Endpoint], rewrite: &RewriteEngine) -> Self {
        let mut index: HashMap<String, Vec<(NodeId, Confidence)>> = HashMap::new();
        for ep in endpoints {
            for cand in rewrite.external_candidates(&ep.key.path) {
                let key = OperationKey {
                    protocol: ep.key.protocol,
                    method: ep.key.method,
                    path: cand.path,
                };
                index
                    .entry(key.signature())
                    .or_default()
                    .push((ep.id.clone(), cand.confidence));
            }
        }
        Matcher { index }
    }

    /// Resolve a single client call to its matching endpoints.
    ///
    /// Per endpoint the highest candidate confidence is kept; then, if any
    /// endpoint matched at `Extracted`, only `Extracted` matches are returned
    /// (exact wins over heuristic). Ties at the same confidence are all kept.
    pub fn resolve(&self, call: &ClientCall) -> Vec<Match> {
        let Some(cands) = self.index.get(&call.key.signature()) else {
            return Vec::new();
        };

        let mut per_endpoint: HashMap<&NodeId, Confidence> = HashMap::new();
        for (eid, conf) in cands {
            let slot = per_endpoint.entry(eid).or_insert(*conf);
            if *conf == Confidence::Extracted {
                *slot = Confidence::Extracted;
            }
        }

        let any_extracted = per_endpoint.values().any(|c| *c == Confidence::Extracted);

        let mut out: Vec<Match> = per_endpoint
            .into_iter()
            .filter(|(_, c)| !any_extracted || *c == Confidence::Extracted)
            .map(|(eid, confidence)| Match {
                client: call.id.clone(),
                endpoint: eid.clone(),
                confidence,
            })
            .collect();
        out.sort_by(|a, b| a.endpoint.0.cmp(&b.endpoint.0));
        out
    }

    /// Resolve every client call, in deterministic (client, endpoint) order.
    pub fn resolve_all(&self, calls: &[ClientCall]) -> Vec<Match> {
        let mut all: Vec<Match> = calls.iter().flat_map(|c| self.resolve(c)).collect();
        all.sort_by(|a, b| {
            a.client
                .0
                .cmp(&b.client.0)
                .then(a.endpoint.0.cmp(&b.endpoint.0))
        });
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::HttpMethod;

    fn ep(id: &str, method: HttpMethod, path: &str) -> Endpoint {
        Endpoint {
            id: NodeId(id.to_string()),
            key: OperationKey::rest(method, path),
        }
    }

    fn call(id: &str, method: HttpMethod, path: &str) -> ClientCall {
        ClientCall {
            id: NodeId(id.to_string()),
            key: OperationKey::rest(method, path),
        }
    }

    fn default_engine() -> RewriteEngine {
        RewriteEngine::new(vec![], vec!["/api".into()], true)
    }

    #[test]
    fn matches_across_the_api_prefix_strip() {
        let m = Matcher::build(
            &[ep("e1", HttpMethod::Get, "/api/users/{id}")],
            &default_engine(),
        );

        // External caller (no /api): matches via the heuristic strip => Inferred.
        let ext = m.resolve(&call("c1", HttpMethod::Get, "/users/123"));
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].endpoint, NodeId("e1".into()));
        assert_eq!(ext[0].confidence, Confidence::Inferred);

        // In-cluster caller (with /api): matches the endpoint's own path => Extracted.
        let int = m.resolve(&call("c2", HttpMethod::Get, "/api/users/999"));
        assert_eq!(int.len(), 1);
        assert_eq!(int[0].confidence, Confidence::Extracted);
    }

    #[test]
    fn explicit_rewrite_makes_external_match_extracted() {
        let eng = RewriteEngine::new(
            vec![crate::rewrite::PrefixRewrite {
                from: "/api".into(),
                to: "".into(),
            }],
            vec!["/api".into()],
            true,
        );
        let m = Matcher::build(&[ep("e1", HttpMethod::Get, "/api/users/{id}")], &eng);
        let ext = m.resolve(&call("c1", HttpMethod::Get, "/users/123"));
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].confidence, Confidence::Extracted);
    }

    #[test]
    fn method_and_shape_mismatches_do_not_match() {
        let m = Matcher::build(
            &[ep("e1", HttpMethod::Get, "/api/users/{id}")],
            &default_engine(),
        );
        assert!(
            m.resolve(&call("c1", HttpMethod::Post, "/users/1"))
                .is_empty()
        );
        assert!(
            m.resolve(&call("c2", HttpMethod::Get, "/orders/1"))
                .is_empty()
        );
    }

    #[test]
    fn exact_match_wins_over_heuristic() {
        // e_exact is served directly at /users/{id} (identity => Extracted);
        // e_gw is served at /api/users/{id} (heuristic strip => Inferred).
        // A client calling /users/5 should resolve only to the exact endpoint.
        let endpoints = [
            ep("e_exact", HttpMethod::Get, "/users/{id}"),
            ep("e_gw", HttpMethod::Get, "/api/users/{id}"),
        ];
        let m = Matcher::build(&endpoints, &default_engine());
        let res = m.resolve(&call("c1", HttpMethod::Get, "/users/5"));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].endpoint, NodeId("e_exact".into()));
        assert_eq!(res[0].confidence, Confidence::Extracted);
    }

    #[test]
    fn same_confidence_ambiguity_is_preserved() {
        // Two distinct endpoints share the exact same external signature.
        let endpoints = [
            ep("e1", HttpMethod::Get, "/users/{id}"),
            ep("e2", HttpMethod::Get, "/users/{userId}"),
        ];
        let m = Matcher::build(&endpoints, &default_engine());
        let res = m.resolve(&call("c1", HttpMethod::Get, "/users/7"));
        let endpoints: Vec<&str> = res.iter().map(|m| m.endpoint.0.as_str()).collect();
        assert_eq!(endpoints, vec!["e1", "e2"]);
        assert!(res.iter().all(|m| m.confidence == Confidence::Extracted));
    }
}
