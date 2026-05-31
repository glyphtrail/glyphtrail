//! Persisted package identity for cross-repo linking (#220 / #221).
//!
//! After `analyze` resolves a repo's Cargo packages, their export indexes, and
//! the external-crate use-sites its files make, it persists them into index
//! meta as JSON under [`META_PACKAGES`] and [`META_EXTERNAL_USES`]. These are
//! the producer and consumer sides that the cross-repo link step (#221) matches
//! against: a consumer's [`ExternalUse`] names a dependency crate, and the
//! link step ties it to the producer repo whose [`IndexedPackage`] has that
//! name, then to the matching [`PackageExport`].
//!
//! The types live in core so every surface (analyze writes them, the link step
//! and MCP read them) shares one definition; loading from a store's meta is the
//! caller's job via [`PackageIdentity::from_meta`], so core stays store-free.

use serde::{Deserialize, Serialize};

use crate::NodeKind;

/// Index-meta key holding the JSON `Vec<IndexedPackage>` (producer side).
pub const META_PACKAGES: &str = "packages";
/// Index-meta key holding the JSON `Vec<ExternalUse>` (consumer side).
pub const META_EXTERNAL_USES: &str = "external_uses";

/// The package ecosystem an identity belongs to. Cross-repo matching is
/// language-agnostic, but a few rules differ per ecosystem (e.g. Go matches by
/// module-path prefix, Python maps a distribution name to its import name), so
/// the tag is recorded for the link step to branch on. Only `Cargo` is produced
/// today; the rest are placeholders for #248/#249/#250.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    /// The default, so indexes written before the ecosystem tag (all Cargo)
    /// deserialize cleanly.
    #[default]
    Cargo,
    Npm,
    Go,
    Python,
}

/// One exported symbol of a package: a definition another crate could name.
/// Visibility is not resolved — a consumer can only reference `pub` items, so a
/// private symbol never appears in any consumer's import path and so never
/// matches. Entries are keyed by symbol `name`; module-path / `pub use`
/// disambiguation is a later refinement covered by crate-level fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageExport {
    pub name: String,
    pub qualified_name: String,
    pub kind: NodeKind,
    pub file: String,
    pub node_id: String,
}

/// A package's identity together with its resolved export index. Ecosystem-
/// neutral: only the name (the cross-repo match key), version, and exports are
/// kept — the dependency list is consumed during analysis to produce
/// [`ExternalUse`]s and is not needed by the link step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedPackage {
    #[serde(default)]
    pub ecosystem: Ecosystem,
    /// Package name as other repos depend on it (the cross-repo match key).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Manifest directory, repo-root-relative and forward-slashed; "" is the
    /// repo root.
    pub dir: String,
    pub exports: Vec<PackageExport>,
}

/// A use-site where one of a repo's files references an external crate: an
/// import whose root path segment matched a declared dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalUse {
    /// Ecosystem of the consuming package (governs how the link step matches).
    #[serde(default)]
    pub ecosystem: Ecosystem,
    /// The consumer package (by name) that owns the importing file.
    pub from_package: String,
    pub from_file: String,
    /// Real crate name of the referenced dependency, rename-resolved.
    pub package: String,
    /// The import path as written, e.g. `widget::go` or `widget::{a, b}`.
    pub path: String,
    /// Node ids of the symbols in `from_file` whose body references the imported
    /// name(s) — the precise use-sites (#236). Empty when none could be
    /// attributed, in which case consumers fall back to file-level landing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from_nodes: Vec<String>,
}

/// A repo's full persisted package identity: the packages it publishes (with
/// their exports) and the external crates its files use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageIdentity {
    pub packages: Vec<IndexedPackage>,
    pub external_uses: Vec<ExternalUse>,
}

impl PackageIdentity {
    /// Parse persisted identity from the two meta JSON blobs (typically
    /// `store.get_meta(META_PACKAGES)` and `store.get_meta(META_EXTERNAL_USES)`).
    /// A missing or malformed blob yields an empty half rather than an error, so
    /// an index produced before identity tracking reads as "no identity".
    pub fn from_meta(packages_json: Option<&str>, external_uses_json: Option<&str>) -> Self {
        let packages = packages_json
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let external_uses = external_uses_json
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        PackageIdentity {
            packages,
            external_uses,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn from_meta_round_trips_persisted_json() {
        let identity = PackageIdentity {
            packages: vec![IndexedPackage {
                ecosystem: Ecosystem::Cargo,
                name: "widget".into(),
                version: Some("1.0.0".into()),
                dir: "crates/widget".into(),
                exports: vec![PackageExport {
                    name: "go".into(),
                    qualified_name: "go".into(),
                    kind: NodeKind::Function,
                    file: "crates/widget/src/lib.rs".into(),
                    node_id: "abc".into(),
                }],
            }],
            external_uses: vec![ExternalUse {
                ecosystem: Ecosystem::Cargo,
                from_package: "app".into(),
                from_file: "crates/app/src/lib.rs".into(),
                package: "widget".into(),
                path: "widget::go".into(),
                from_nodes: vec!["node-caller".into()],
            }],
        };
        let packages_json = serde_json::to_string(&identity.packages).unwrap();
        let uses_json = serde_json::to_string(&identity.external_uses).unwrap();
        let loaded = PackageIdentity::from_meta(Some(&packages_json), Some(&uses_json));
        check!(loaded == identity);
    }

    #[test]
    fn from_meta_defaults_ecosystem_for_pre_tag_indexes() {
        // A `packages` blob written before the ecosystem tag (no `ecosystem`
        // field, with the old flattened name/version) still deserializes.
        let old = r#"[{"name":"widget","version":"1.0.0","dir":"","exports":[]}]"#;
        let id = PackageIdentity::from_meta(Some(old), None);
        check!(id.packages.len() == 1);
        check!(id.packages[0].ecosystem == Ecosystem::Cargo);
        check!(id.packages[0].name == "widget");
    }

    #[test]
    fn from_meta_tolerates_missing_and_malformed() {
        check!(PackageIdentity::from_meta(None, None) == PackageIdentity::default());
        check!(
            PackageIdentity::from_meta(Some("not json"), Some("{")) == PackageIdentity::default()
        );
    }
}
