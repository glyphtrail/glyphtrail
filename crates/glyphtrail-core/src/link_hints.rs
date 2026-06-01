//! Manual cross-repo connection hints (#281).
//!
//! Auto-linking ties a consumer's dependency to the producer repo by package
//! name. Some real cross-repo relationships aren't expressible that way — a
//! service in one repo called over HTTP by a client in another, with no shared
//! package. Users declare those by hand in a small TOML file, and they feed the
//! federated link table alongside the auto-resolved links.
//!
//! Two overlays, unioned: a **team-shared** committed file at the repo root
//! (`glyphtrail.links.toml`) plus a **user-private** override inside the
//! gitignored index dir (`.glyphtrail/links.toml`). Each `[[links]]` entry names
//! a `from` (the consumer/caller) and a `to` (the producer/callee) — changing
//! `to` impacts `from`. Each side's `repo` defaults to `.` (the repo whose file
//! this is), so you only ever name the *other* repo; a side without a `symbol`
//! is a coarse whole-repo link.
//!
//! ```toml
//! # in web-client's glyphtrail.links.toml: we call user-svc's endpoint
//! [[links]]
//! from = { symbol = "fetchUser" }              # repo "." = here
//! to   = { repo = "user-svc", symbol = "get_user" }
//!
//! # coarse: we depend on user-svc as a whole
//! [[links]]
//! to = { repo = "user-svc" }                   # from "." implied
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Committed, team-shared hints file at the repo root.
pub const HINTS_FILE: &str = "glyphtrail.links.toml";
/// User-private override inside the (gitignored) index dir.
pub const LOCAL_HINTS_FILE: &str = "links.toml";

/// One end of a hinted link. `repo` defaults to `.` (the declaring repo); a
/// missing `symbol` means the whole repo (a coarse link).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
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
        self.repo.is_none() && self.symbol.is_none()
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

/// The parsed hints file(s) for one repo.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LinkHints {
    #[serde(default)]
    pub links: Vec<LinkHint>,
}

impl LinkHints {
    /// Load and union a repo's committed root file and its gitignored local
    /// override. A missing or malformed file contributes nothing (best-effort);
    /// `repo_root` is the repo's working-tree root, `index_dir` its `.glyphtrail`.
    pub fn load(repo_root: &Path, index_dir: &Path) -> Self {
        let mut links = Vec::new();
        for path in [repo_root.join(HINTS_FILE), index_dir.join(LOCAL_HINTS_FILE)] {
            links.extend(Self::load_file(&path).links);
        }
        LinkHints { links }
    }

    /// Load a single hints file, returning empty when it's absent or malformed.
    /// Used by the `link` editing commands, which operate on one overlay at a time.
    pub fn load_file(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write this set back to `path` (creating parent dirs), as TOML.
    pub fn save_file(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self).unwrap_or_else(|_| "links = []\n".to_string());
        std::fs::write(path, toml)
    }
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
}
