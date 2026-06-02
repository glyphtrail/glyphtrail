//! Manual cross-repo connection hints (#281).
//!
//! Auto-linking ties a consumer's dependency to the producer repo by package
//! name. Some real cross-repo relationships aren't expressible that way — a
//! service in one repo called over HTTP by a client in another, with no shared
//! package. Users declare those by hand as `[[links]]` in the unified config
//! (`glyphtrail.toml` committed + `.glyphtrail/glyphtrail.toml` personal,
//! unioned; see [`crate::config::Config::links`]), feeding the federated link
//! table alongside the auto-resolved links.
//!
//! Each `[[links]]` entry names a `from` (the consumer/caller) and a `to` (the
//! producer/callee) — changing `to` impacts `from`. Each side's `repo` defaults
//! to `.` (the repo whose file this is), so you only ever name the *other* repo;
//! a side without a `symbol` is a coarse whole-repo link.
//!
//! ```toml
//! # in web-client's glyphtrail.toml: we call user-svc's endpoint
//! [[links]]
//! from = { symbol = "fetchUser" }              # repo "." = here
//! to   = { repo = "user-svc", symbol = "get_user" }
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::registry::Registry;

/// Whether a link `repo` value is a filesystem path (`./`, `../`, or absolute)
/// rather than a registry name — only the path forms are resolved against the
/// filesystem; a bare slashed value (a GitLab `group/sub/repo`) stays a name.
pub fn is_path_ref(repo: &str) -> bool {
    repo.starts_with("./") || repo.starts_with("../") || Path::new(repo).is_absolute()
}

/// Resolve a link side's `repo` to a registered repo NAME, or `None` when it
/// names no registered repo. `None`/`"."` is the declaring repo (`owner_name`,
/// always valid); a `./`/`../`/absolute value is a path relative to `owner_root`,
/// mapped to the registered repo rooted there; anything else is a name, checked
/// against the registry. Shared by federated impact (which falls back to the
/// verbatim value) and the link tooling (which treats `None` as a dead link).
pub fn resolved_link_repo(
    repo: &Option<String>,
    owner_name: &str,
    owner_root: &Path,
    registry: &Registry,
) -> Option<String> {
    match repo.as_deref() {
        None | Some(".") => Some(owner_name.to_string()),
        Some(r) if is_path_ref(r) => owner_root.join(r).canonicalize().ok().and_then(|abs| {
            registry
                .repos
                .iter()
                .find(|e| {
                    e.roots()
                        .any(|root| root.canonicalize().map(|c| c == abs).unwrap_or(false))
                })
                .map(|e| e.name.clone())
        }),
        Some(r) => registry.get(r).map(|e| e.name.clone()),
    }
}

/// Pre-unification standalone hints file at the repo root, still read for
/// back-compat and folded into `glyphtrail.toml` on the next `link`/`config` edit.
pub const HINTS_FILE: &str = "glyphtrail.links.toml";
/// Pre-unification personal hints file inside the index dir (`.glyphtrail/links.toml`).
pub const LOCAL_HINTS_FILE: &str = "links.toml";

/// One end of a hinted link. `repo` defaults to `.` (the declaring repo); a
/// side with neither `symbol` nor `endpoint` means the whole repo (a coarse
/// link). `endpoint` names a REST operation (`"POST /signin"`, or `"/signin"`
/// for any method) and resolves to the matching `Endpoint`/`ClientCall` nodes
/// by signature — the precise call↔endpoint pin when a path is dynamic and a
/// symbol name isn't enough (#407).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl LinkEnd {
    /// The repo this end refers to, resolving `.`/absent to `owner` (the repo
    /// whose hints file this came from).
    pub fn repo_or(&self, owner: &str) -> String {
        match self.repo.as_deref() {
            None | Some(".") => owner.to_string(),
            Some(r) => r.to_string(),
        }
    }

    /// Whether this end carries nothing (the default "here, whole repo"), so it
    /// can be omitted when serializing a hint.
    pub fn is_empty(&self) -> bool {
        self.repo.is_none() && self.symbol.is_none() && self.endpoint.is_none()
    }
}

/// A declared cross-repo edge: `from` (consumer) depends on `to` (producer), so
/// a change to `to` impacts `from`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkHint {
    #[serde(default, skip_serializing_if = "LinkEnd::is_empty")]
    pub from: LinkEnd,
    #[serde(default, skip_serializing_if = "LinkEnd::is_empty")]
    pub to: LinkEnd,
}

/// A `links = [...]` document (the `[[links]]` array), used to parse a hints
/// file. The live hints are carried on [`crate::config::Config::links`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LinkHints {
    #[serde(default)]
    pub links: Vec<LinkHint>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn parses_symbol_and_coarse_links_with_repo_defaulting_to_here() {
        let toml = r#"
            [[links]]
            from = { symbol = "fetchUser" }
            to   = { repo = "user-svc", symbol = "get_user" }

            [[links]]
            to = { repo = "user-svc" }
        "#;
        let hints: LinkHints = toml::from_str(toml).unwrap();
        check!(hints.links.len() == 2);

        let precise = &hints.links[0];
        check!(precise.from.repo_or("web-client") == "web-client"); // "." -> here
        check!(precise.from.symbol.as_deref() == Some("fetchUser"));
        check!(precise.to.repo_or("web-client") == "user-svc");
        check!(precise.to.symbol.as_deref() == Some("get_user"));

        let coarse = &hints.links[1];
        check!(coarse.from.repo_or("web-client") == "web-client"); // omitted -> here
        check!(coarse.from.symbol.is_none());
        check!(coarse.to.symbol.is_none()); // whole-repo
    }

    #[test]
    fn parses_endpoint_link_pinning_a_call_to_a_route() {
        let toml = r#"
            [[links]]
            from = { symbol = "login" }
            to   = { repo = "backend", endpoint = "POST /signin" }
        "#;
        let hints: LinkHints = toml::from_str(toml).unwrap();
        let h = &hints.links[0];
        check!(h.from.symbol.as_deref() == Some("login"));
        check!(h.to.endpoint.as_deref() == Some("POST /signin"));
        // An endpoint-only side is not empty (so it serializes + resolves).
        check!(!h.to.is_empty());
        check!(LinkEnd::default().is_empty());
        // Round-trips, keeping the endpoint key.
        let back = toml::to_string(&hints).unwrap();
        check!(back.contains("endpoint = \"POST /signin\""));
    }

    #[test]
    fn is_path_ref_distinguishes_paths_from_names() {
        check!(is_path_ref("./mmh/web"));
        check!(is_path_ref("../web"));
        check!(is_path_ref("/abs/web"));
        check!(!is_path_ref("backend"));
        // A slashed *name* (e.g. a GitLab nested repo) is not a path.
        check!(!is_path_ref("group/subgroup/repo"));
    }

    #[test]
    fn resolved_link_repo_maps_path_and_name_to_registered_or_none() {
        use crate::registry::RegistryEntry;
        let base = std::env::temp_dir().join(format!(
            "gt-resolvelink-{}-{}",
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
            visibility: crate::registry::Visibility::default(),
        });
        let r =
            |s: Option<&str>| resolved_link_repo(&s.map(String::from), "owner", &meta, &registry);
        check!(r(None) == Some("owner".into())); // "." / absent -> declaring repo
        check!(r(Some(".")) == Some("owner".into()));
        check!(r(Some("subrepo")) == Some("subrepo".into())); // a *registered* name
        check!(r(Some("nope")).is_none()); // an unregistered name -> dead
        check!(r(Some("./sub")) == Some("subrepo".into())); // a path -> the name
        check!(r(Some("./nope")).is_none()); // an unresolvable path -> dead
        std::fs::remove_dir_all(&base).ok();
    }
}
