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

use crate::{CargoPackage, NodeKind};

/// Index-meta key holding the JSON `Vec<IndexedPackage>` (producer side).
pub const META_PACKAGES: &str = "packages";
/// Index-meta key holding the JSON `Vec<ExternalUse>` (consumer side).
pub const META_EXTERNAL_USES: &str = "external_uses";

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

/// A package's identity together with its resolved export index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedPackage {
    /// Manifest directory, repo-root-relative and forward-slashed; "" is the
    /// repo root.
    pub dir: String,
    #[serde(flatten)]
    pub package: CargoPackage,
    pub exports: Vec<PackageExport>,
}

/// A use-site where one of a repo's files references an external crate: an
/// import whose root path segment matched a declared dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalUse {
    /// The consumer package (by name) that owns the importing file.
    pub from_package: String,
    pub from_file: String,
    /// Real crate name of the referenced dependency, rename-resolved.
    pub package: String,
    /// The import path as written, e.g. `widget::go` or `widget::{a, b}`.
    pub path: String,
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
                dir: "crates/widget".into(),
                package: crate::parse_cargo_manifest(
                    "[package]\nname = \"widget\"\nversion = \"1.0.0\"\n",
                )
                .unwrap()
                .unwrap(),
                exports: vec![PackageExport {
                    name: "go".into(),
                    qualified_name: "go".into(),
                    kind: NodeKind::Function,
                    file: "crates/widget/src/lib.rs".into(),
                    node_id: "abc".into(),
                }],
            }],
            external_uses: vec![ExternalUse {
                from_package: "app".into(),
                from_file: "crates/app/src/lib.rs".into(),
                package: "widget".into(),
                path: "widget::go".into(),
            }],
        };
        let packages_json = serde_json::to_string(&identity.packages).unwrap();
        let uses_json = serde_json::to_string(&identity.external_uses).unwrap();
        let loaded = PackageIdentity::from_meta(Some(&packages_json), Some(&uses_json));
        check!(loaded == identity);
    }

    #[test]
    fn from_meta_tolerates_missing_and_malformed() {
        check!(PackageIdentity::from_meta(None, None) == PackageIdentity::default());
        check!(
            PackageIdentity::from_meta(Some("not json"), Some("{")) == PackageIdentity::default()
        );
    }
}
