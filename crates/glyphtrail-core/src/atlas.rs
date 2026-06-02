//! Atlas (#329): a private, local-only global archaeology index across every
//! registered repo. This module holds the shared, store-agnostic pieces — the
//! path resolver, the `Commit` side-table record, and the atlas config. The
//! store schema/accessors live in `glyphtrail-store`; the lifecycle and query
//! commands in the CLI. Atlas writes only under `~/.glyphtrail/atlas/` — no
//! network, no export.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::NodeId;

/// The atlas store directory (`~/.glyphtrail/atlas/`), or `None` without a home
/// directory. Mirrors [`crate::default_groups_path`] and runs the pre-rename
/// home migration first. The directory exists only after an explicit
/// `atlas init`; its absence means atlas is disabled.
pub fn default_atlas_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?);
    crate::registry::migrate_legacy_home(&home); // silent pre-rename upgrade (#293)
    Some(home.join(".glyphtrail").join("atlas"))
}

/// A row of the `Commit` side-table (#330): commit attributes keyed by the
/// `Commit` node's id and indexed on `committed_at`, mirroring `ApiOp`.
/// `in_bounds` carries the date-window state, so narrowing the window later
/// re-marks stored commits out of bounds rather than deleting them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    /// The `Commit` node's id this row belongs to.
    pub node_id: NodeId,
    /// Full commit hash.
    pub hash: String,
    /// Author email (raw, as recorded by git).
    pub author_email: String,
    /// Commit timestamp, unix seconds.
    pub committed_at: i64,
    /// Commit subject (first line), secret-scrubbed before storage.
    pub subject: String,
    /// Within the active date window. Default `true`.
    pub in_bounds: bool,
}

/// The atlas config file (`~/.glyphtrail/atlas/atlas.toml`). #330 reads
/// `[window]`; commit ingestion (#331) extends the same file with `[me]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AtlasConfig {
    #[serde(default)]
    pub window: Window,
}

/// `[window]` — the optional global date bounds on what atlas indexes. Absent
/// keys mean no bound on that side.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Window {
    /// Earliest commit date to index (e.g. `2015-01-01`), inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest: Option<String>,
    /// Latest commit date to index, inclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
}

impl Window {
    /// Whether any bound is set.
    pub fn is_set(&self) -> bool {
        self.earliest.is_some() || self.latest.is_some()
    }
}

impl AtlasConfig {
    /// Load `atlas.toml` from `atlas_dir`; the default (no window) when absent.
    pub fn load(atlas_dir: &Path) -> crate::Result<AtlasConfig> {
        let path = atlas_dir.join("atlas.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|source| crate::error::CoreError::ConfigParse {
                    path,
                    source: Box::new(source),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AtlasConfig::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn config_load_reads_window_and_defaults_to_none() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gt-atlas-cfg-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        // Absent file -> default (no window).
        check!(!AtlasConfig::load(&dir).unwrap().window.is_set());
        std::fs::write(
            dir.join("atlas.toml"),
            "[window]\nearliest = \"2015-01-01\"\n",
        )
        .unwrap();
        let cfg = AtlasConfig::load(&dir).unwrap();
        check!(cfg.window.earliest.as_deref() == Some("2015-01-01"));
        check!(cfg.window.latest.is_none());
        check!(cfg.window.is_set());
        std::fs::remove_dir_all(&dir).ok();
    }
}
