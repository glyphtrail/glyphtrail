//! Atlas (#329): a private, local-only global archaeology index across every
//! registered repo. This module holds the shared, store-agnostic pieces — the
//! path resolver, the `Commit` side-table record, and the atlas config. The
//! store schema/accessors live in `glyphtrail-store`; the lifecycle and query
//! commands in the CLI. Atlas writes only under `~/.glyphtrail/atlas/` — no
//! network, no export.

use std::collections::BTreeMap;
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
/// `Commit` node's id, carrying `committed_at` for time-ordered queries,
/// mirroring `ApiOp`. `in_bounds` carries the date-window state, so narrowing
/// the window later re-marks stored commits out of bounds rather than deleting.
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

/// One row of the atlas timeline (#333): a commit joined to its repo name and
/// touched-file count for chronological display. Built by the store; visibility
/// / author filtering is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasTimelineRow {
    pub commit: CommitMeta,
    /// Registry name of the repo this commit belongs to.
    pub repo: String,
    /// How many files the commit touched.
    pub touched: u32,
}

/// Format a unix-second timestamp as a `YYYY-MM-DD` UTC calendar date — the
/// inverse of [`date_to_epoch`] (Howard Hinnant's `civil_from_days`), so the
/// timeline reads dates back without a time-crate dependency.
pub fn format_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The atlas config file (`~/.glyphtrail/atlas/atlas.toml`). #330 reads
/// `[window]`; commit ingestion (#331) adds `[me]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AtlasConfig {
    #[serde(default)]
    pub window: Window,
    #[serde(default)]
    pub me: MeConfig,
}

/// `[me]` — who "I" am, so `atlas sync` can keep only my own commits by default
/// and roll every raw author of mine up to one `Identity` (#331). An address
/// matches if it is listed in `emails`, or sits at one of my `domains` (catching
/// forgotten local-parts on a domain I own). Seeded best-effort from
/// `git config user.email` and the registry contributors; user-curated.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MeConfig {
    /// Exact addresses that are mine (matched case-insensitively).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<String>,
    /// Domains I own; any address at one of them is mine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
}

impl MeConfig {
    /// Whether any identity is configured.
    pub fn is_set(&self) -> bool {
        !self.emails.is_empty() || !self.domains.is_empty()
    }

    /// Whether `email` resolves to me: an exact (case-insensitive) address match,
    /// or any address at one of my owned domains.
    pub fn matches(&self, email: &str) -> bool {
        let email = email.trim().to_ascii_lowercase();
        if self.emails.iter().any(|m| m.eq_ignore_ascii_case(&email)) {
            return true;
        }
        match email.rsplit_once('@') {
            Some((_, domain)) => self.domains.iter().any(|d| d.eq_ignore_ascii_case(domain)),
            None => false,
        }
    }

    /// A display address for my unified identity: the first configured email, or
    /// `me@<first domain>` when only domains are known.
    pub fn display(&self) -> Option<String> {
        self.emails
            .first()
            .cloned()
            .or_else(|| self.domains.first().map(|d| format!("me@{d}")))
    }
}

/// Per-repo last-synced HEAD (`~/.glyphtrail/atlas/heads.json`), keyed by
/// registry name (#331). Lives beside the atlas store so wiping the atlas dir
/// also clears the watermarks, forcing a clean full re-walk. Lets each `atlas
/// sync` ingest only `<saved head>..HEAD`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AtlasHeads {
    #[serde(default)]
    pub heads: BTreeMap<String, String>,
}

impl AtlasHeads {
    /// Load `heads.json` from `atlas_dir`; an empty map when absent.
    pub fn load(atlas_dir: &Path) -> crate::Result<AtlasHeads> {
        let path = atlas_dir.join("heads.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|source| crate::error::CoreError::RegistryParse { path, source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AtlasHeads::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist to `heads.json` under `atlas_dir`. Atomic (staged in a
    /// process-unique temp file, then renamed) so an interrupted write never
    /// truncates the watermark; mirrors [`crate::Registry::save`].
    pub fn save(&self, atlas_dir: &Path) -> crate::Result<()> {
        let path = atlas_dir.join("heads.json");
        let json = serde_json::to_string_pretty(self).map_err(|source| {
            crate::error::CoreError::RegistryParse {
                path: path.clone(),
                source,
            }
        })?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// The saved HEAD for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.heads.get(name).map(String::as_str)
    }

    /// Record `head` as the last-synced commit for `name`.
    pub fn set(&mut self, name: &str, head: &str) {
        self.heads.insert(name.to_string(), head.to_string());
    }
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

    /// The window as inclusive unix-second bounds (UTC): `earliest` at the start
    /// of its day, `latest` at the end (23:59:59), so a calendar `latest` covers
    /// the whole day. `Ok((None, None))` when unset. `Err(date)` names the first
    /// unparseable `YYYY-MM-DD`.
    pub fn epoch_bounds(&self) -> std::result::Result<(Option<i64>, Option<i64>), String> {
        let since = match &self.earliest {
            Some(d) => Some(date_to_epoch(d, false).ok_or_else(|| d.clone())?),
            None => None,
        };
        let until = match &self.latest {
            Some(d) => Some(date_to_epoch(d, true).ok_or_else(|| d.clone())?),
            None => None,
        };
        Ok((since, until))
    }
}

/// Unix seconds for a `YYYY-MM-DD` calendar date (UTC). `end_of_day` selects
/// 23:59:59 (an inclusive upper bound) over 00:00:00. `None` for a malformed
/// date. Matches `committed_at`, which `git`'s `%ct` reports as a UTC epoch.
fn date_to_epoch(date: &str, end_of_day: bool) -> Option<i64> {
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    let base = days_from_civil(y, m, d) * 86_400;
    Some(if end_of_day { base + 86_399 } else { base })
}

/// Days in month `m` of year `y` (1-based), leap-year aware, so an impossible
/// calendar date (e.g. `2015-02-31`) is rejected rather than silently shifted.
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to a proleptic-Gregorian `y-m-d` (Howard Hinnant's
/// `days_from_civil`); negative before the epoch.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
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

    #[test]
    fn me_matches_exact_email_and_owned_domain() {
        let me = MeConfig {
            emails: vec!["Ada@Example.com".into()],
            domains: vec!["mine.dev".into()],
        };
        // Exact match, case-insensitive.
        check!(me.matches("ada@example.com"));
        // Any local-part on an owned domain.
        check!(me.matches("forgotten@mine.dev"));
        check!(me.matches("OTHER@MINE.DEV"));
        // A stranger at neither.
        check!(!me.matches("someone@elsewhere.com"));
        // A domain look-alike is not owned.
        check!(!me.matches("x@notmine.dev"));
        check!(!me.matches("no-at-sign"));
        check!(me.display().as_deref() == Some("Ada@Example.com"));
    }

    #[test]
    fn window_epoch_bounds_are_inclusive_utc_days() {
        // 1970-01-01 is the epoch; latest spans to the end of its day.
        let w = Window {
            earliest: Some("1970-01-01".into()),
            latest: Some("1970-01-01".into()),
        };
        check!(w.epoch_bounds().unwrap() == (Some(0), Some(86_399)));
        // A known date: 2015-01-01 00:00:00 UTC.
        let w = Window {
            earliest: Some("2015-01-01".into()),
            latest: None,
        };
        check!(w.epoch_bounds().unwrap() == (Some(1_420_070_400), None));
        // Unset -> no bounds; malformed -> the offending date.
        check!(Window::default().epoch_bounds().unwrap() == (None, None));
        let bad = Window {
            earliest: Some("2015-13-40".into()),
            latest: None,
        };
        check!(bad.epoch_bounds() == Err("2015-13-40".to_string()));
        // Impossible calendar days are rejected, leap-years respected.
        check!(date_to_epoch("2015-02-31", false).is_none());
        check!(date_to_epoch("2015-02-29", false).is_none()); // 2015 is not a leap year
        check!(date_to_epoch("2016-02-29", false).is_some()); // 2016 is
    }

    #[test]
    fn format_date_inverts_date_to_epoch() {
        for date in [
            "1970-01-01",
            "2015-01-01",
            "2016-02-29",
            "2026-06-02",
            "1999-12-31",
        ] {
            let epoch = date_to_epoch(date, false).unwrap();
            check!(format_date(epoch) == date);
        }
        // End-of-day still reads back as the same calendar date.
        check!(format_date(date_to_epoch("2020-07-15", true).unwrap()) == "2020-07-15");
    }

    #[test]
    fn atlas_heads_round_trip() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gt-atlas-heads-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        check!(AtlasHeads::load(&dir).unwrap().get("repo").is_none());
        let mut heads = AtlasHeads::default();
        heads.set("repo", "deadbeef");
        heads.save(&dir).unwrap();
        check!(AtlasHeads::load(&dir).unwrap().get("repo") == Some("deadbeef"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
