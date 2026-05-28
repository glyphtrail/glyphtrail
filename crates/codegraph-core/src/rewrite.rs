//! Deployment path-rewrite engine.
//!
//! Server endpoints live at *internal* paths (e.g. `/api/users/{id}`), but the
//! clients that call them use *external* paths (`/users/{id}`) because a gateway
//! such as Envoy rewrites the path on the way in. To match a client call to the
//! endpoint that answers it, the matcher reduces an endpoint's internal path to
//! the set of external paths a client could plausibly use.
//!
//! Two sources feed that set, with different trust:
//! - **Explicit rewrites** from config describe the real gateway mapping and are
//!   tagged [`Confidence::Extracted`].
//! - A **heuristic** that strips well-known gateway prefixes (e.g. `/api`) runs
//!   by default and is tagged [`Confidence::Inferred`].

use serde::Deserialize;

use crate::api::normalize_path;
use crate::model::Confidence;

/// A prefix-based path rewrite. `from` is matched on a path-segment boundary and
/// replaced with `to`. An empty `from` prepends `to`; an empty `to` strips
/// `from`. e.g. `{ from = "/api", to = "" }` maps `/api/users` → `/users`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixRewrite {
    pub from: String,
    pub to: String,
}

impl PrefixRewrite {
    /// Apply this rewrite to `path`, returning the rewritten path or `None` if
    /// `from` does not match on a segment boundary.
    pub fn apply(&self, path: &str) -> Option<String> {
        let from = normalize_prefix(&self.from);
        let to = normalize_prefix(&self.to);
        let p = normalize_path(path);

        if from.is_empty() {
            return Some(normalize_path(&format!("{to}{p}")));
        }
        if p == from {
            return Some(if to.is_empty() { "/".to_string() } else { to });
        }
        let with_slash = format!("{from}/");
        if let Some(rest) = p.strip_prefix(&with_slash) {
            return Some(normalize_path(&format!("{to}/{rest}")));
        }
        None
    }
}

/// Normalize a path prefix: trim surrounding slashes/space; empty stays empty,
/// otherwise gets a single leading slash and no trailing slash.
fn normalize_prefix(s: &str) -> String {
    let t = s.trim().trim_matches('/');
    if t.is_empty() {
        String::new()
    } else {
        format!("/{t}")
    }
}

/// An external-facing path an endpoint may be reached at, with how confident we
/// are that a client using it really hits this endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteCandidate {
    pub path: String,
    pub confidence: Confidence,
}

/// Maps internal endpoint paths to the external paths clients may use.
#[derive(Debug, Clone)]
pub struct RewriteEngine {
    rewrites: Vec<PrefixRewrite>,
    gateway_prefixes: Vec<String>,
    heuristic: bool,
}

impl RewriteEngine {
    pub fn new(
        rewrites: Vec<PrefixRewrite>,
        gateway_prefixes: Vec<String>,
        heuristic: bool,
    ) -> Self {
        Self {
            rewrites,
            gateway_prefixes,
            heuristic,
        }
    }

    pub fn from_config(cfg: &crate::config::ApiConfig) -> Self {
        Self::new(
            cfg.rewrites.clone(),
            cfg.gateway_prefixes.clone(),
            cfg.heuristic_prefix_strip,
        )
    }

    /// The external paths an endpoint at `internal_path` may be reached at.
    ///
    /// Always includes the path itself (a direct/un-gatewayed caller) plus every
    /// applicable explicit rewrite, both `Extracted`; and, when the heuristic is
    /// enabled, each known gateway prefix stripped, as `Inferred`. Results are
    /// deduplicated keeping the highest confidence, in deterministic path order.
    pub fn external_candidates(&self, internal_path: &str) -> Vec<RewriteCandidate> {
        let mut best: std::collections::BTreeMap<String, Confidence> = Default::default();

        upsert(&mut best, internal_path, Confidence::Extracted);
        for r in &self.rewrites {
            if let Some(ext) = r.apply(internal_path) {
                upsert(&mut best, &ext, Confidence::Extracted);
            }
        }
        if self.heuristic {
            for pre in &self.gateway_prefixes {
                let rule = PrefixRewrite {
                    from: pre.clone(),
                    to: String::new(),
                };
                if let Some(ext) = rule.apply(internal_path) {
                    upsert(&mut best, &ext, Confidence::Inferred);
                }
            }
        }

        best.into_iter()
            .map(|(path, confidence)| RewriteCandidate { path, confidence })
            .collect()
    }
}

/// Insert a normalized candidate, upgrading to `Extracted` if seen both ways.
fn upsert(map: &mut std::collections::BTreeMap<String, Confidence>, path: &str, conf: Confidence) {
    let p = normalize_path(path);
    let entry = map.entry(p).or_insert(conf);
    if conf == Confidence::Extracted {
        *entry = Confidence::Extracted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(c: &[RewriteCandidate]) -> Vec<&str> {
        c.iter().map(|r| r.path.as_str()).collect()
    }

    #[test]
    fn strip_and_add_and_replace_apply_on_boundaries() {
        let strip = PrefixRewrite {
            from: "/api".into(),
            to: "".into(),
        };
        assert_eq!(
            strip.apply("/api/users/{id}").as_deref(),
            Some("/users/{id}")
        );
        assert_eq!(strip.apply("/api").as_deref(), Some("/"));
        // No partial-segment match.
        assert_eq!(strip.apply("/apiv2/x"), None);

        let add = PrefixRewrite {
            from: "".into(),
            to: "/api".into(),
        };
        assert_eq!(add.apply("/users").as_deref(), Some("/api/users"));

        let replace = PrefixRewrite {
            from: "/internal".into(),
            to: "/v2".into(),
        };
        assert_eq!(replace.apply("/internal/x").as_deref(), Some("/v2/x"));
        assert_eq!(replace.apply("/other"), None);
    }

    #[test]
    fn default_heuristic_strips_api_prefix_as_inferred() {
        let eng = RewriteEngine::new(vec![], vec!["/api".into()], true);
        let cands = eng.external_candidates("/api/users/{id}");
        assert_eq!(paths(&cands), vec!["/api/users/{id}", "/users/{id}"]);
        // identity is extracted, heuristic strip is inferred
        let inferred: Vec<_> = cands
            .iter()
            .filter(|c| c.confidence == Confidence::Inferred)
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(inferred, vec!["/users/{id}"]);
    }

    #[test]
    fn explicit_rule_outranks_heuristic_for_same_path() {
        // Both the explicit strip and the heuristic produce "/users"; the
        // explicit (Extracted) confidence must win.
        let eng = RewriteEngine::new(
            vec![PrefixRewrite {
                from: "/api".into(),
                to: "".into(),
            }],
            vec!["/api".into()],
            true,
        );
        let cands = eng.external_candidates("/api/users");
        let users = cands.iter().find(|c| c.path == "/users").unwrap();
        assert_eq!(users.confidence, Confidence::Extracted);
    }

    #[test]
    fn heuristic_disabled_yields_identity_only() {
        let eng = RewriteEngine::new(vec![], vec!["/api".into()], false);
        let cands = eng.external_candidates("/api/users");
        assert_eq!(paths(&cands), vec!["/api/users"]);
    }
}
