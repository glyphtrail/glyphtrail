//! Atlas (#329): a private, local-only global archaeology index across every
//! registered repo. This module holds the shared, store-agnostic pieces — the
//! path resolver, the `Commit` side-table record, and the atlas config. The
//! store schema/accessors live in `glyphtrail-store`; the lifecycle and query
//! commands in the CLI. Atlas writes only under `~/.glyphtrail/atlas/` — no
//! network, no export.

use std::collections::{BTreeMap, BTreeSet};
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

/// Words too generic to be a useful topic — common git verbs and English filler
/// that would otherwise dominate every commit (#334).
const TOPIC_STOPWORDS: &[&str] = &[
    "add",
    "added",
    "adds",
    "fix",
    "fixed",
    "fixes",
    "update",
    "updated",
    "updates",
    "remove",
    "removed",
    "removes",
    "delete",
    "deleted",
    "refactor",
    "rename",
    "renamed",
    "move",
    "moved",
    "bump",
    "merge",
    "revert",
    "wip",
    "use",
    "used",
    "make",
    "made",
    "set",
    "get",
    "init",
    "clean",
    "cleanup",
    "improve",
    "improved",
    "change",
    "changed",
    "changes",
    "support",
    "implement",
    "handle",
    "allow",
    "avoid",
    "ensure",
    "the",
    "and",
    "for",
    "with",
    "from",
    "into",
    "this",
    "that",
    "when",
    "then",
    "also",
    "via",
    "not",
    "but",
    "are",
    "was",
    "has",
    "had",
    "can",
    "all",
    "new",
    "now",
    "one",
    "two",
    "more",
    "less",
    "some",
    "any",
    "out",
    "off",
    "per",
    "you",
    "your",
    "its",
    "their",
    "test",
    "tests",
    "todo",
];

/// Directory segments too generic to name an area of work (#334).
const TOPIC_GENERIC_DIRS: &[&str] = &[
    "src",
    "lib",
    "test",
    "tests",
    "crates",
    "crate",
    "app",
    "apps",
    "pkg",
    "packages",
    "internal",
    "cmd",
    "main",
    "mod",
    "index",
    "bin",
    "dist",
    "build",
    "target",
    "node_modules",
    "vendor",
    "docs",
    "doc",
    "examples",
    "example",
    "include",
    "assets",
    "static",
];

/// The most topics any single commit contributes — bounds noise (#334).
const MAX_TOPICS_PER_COMMIT: usize = 12;

/// Derive heuristic topic tags for a commit (#334) from its scrubbed subject
/// (significant keywords), its touched directories (areas of the tree), and the
/// languages of its touched files. Lower-cased, de-duplicated, stop-worded, and
/// capped. No network, no LLM — enrichment is a later option.
/// A bounded, order-stable digest of a commit's changed file paths — its top
/// directory segments and file extensions by frequency — so a commit with a sparse
/// message ("Initial commit" that adds 500 images) still embeds with meaning,
/// without the path list blowing the embedding model's token budget (#338). Capped
/// at 12 directories + 8 extensions, regardless of how many files changed.
pub fn paths_digest(paths: &[String]) -> String {
    use std::collections::HashMap;
    let mut dirs: HashMap<String, usize> = HashMap::new();
    let mut exts: HashMap<String, usize> = HashMap::new();
    for p in paths {
        let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        if let Some((file, dir_segs)) = segs.split_last() {
            for d in dir_segs {
                *dirs.entry(d.to_ascii_lowercase()).or_default() += 1;
            }
            // Extension: after the last dot, short, and not the whole name (so a
            // dotfile like `.gitignore` doesn't count `gitignore` as an extension).
            if let Some((stem, ext)) = file.rsplit_once('.')
                && !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 8
            {
                *exts.entry(ext.to_ascii_lowercase()).or_default() += 1;
            }
        }
    }
    let top = |m: HashMap<String, usize>, k: usize| -> Vec<String> {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter().take(k).map(|(name, _)| name).collect()
    };
    let mut tokens = top(dirs, 12);
    tokens.extend(top(exts, 8));
    tokens.join(" ")
}

pub fn derive_topics(subject: &str, files: &[String]) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut topics: BTreeSet<String> = BTreeSet::new();

    // Subject keywords.
    for token in subject.split(|c: char| !c.is_alphanumeric()) {
        let token = token.to_ascii_lowercase();
        if token.len() >= 3
            && token.chars().any(|c| c.is_alphabetic())
            && !TOPIC_STOPWORDS.contains(&token.as_str())
        {
            topics.insert(token);
        }
    }

    // Touched directory segments + file languages.
    for file in files {
        let path = std::path::Path::new(file);
        if let Some(parent) = path.parent() {
            for component in parent.components() {
                if let std::path::Component::Normal(seg) = component {
                    let seg = seg.to_string_lossy().to_ascii_lowercase();
                    // Directory names are curated and low-noise, so accept short
                    // areas like `ui`/`io`/`db` that the subject filter drops.
                    if seg.len() >= 2
                        && seg.chars().any(|c| c.is_alphabetic())
                        && !TOPIC_GENERIC_DIRS.contains(&seg.as_str())
                    {
                        topics.insert(seg);
                    }
                }
            }
        }
        if let Some(lang) = crate::lang::Language::from_path(path) {
            topics.insert(lang.name().to_ascii_lowercase());
        }
    }

    topics.into_iter().take(MAX_TOPICS_PER_COMMIT).collect()
}

/// How to filter an atlas timeline (#333/#335) — shared by the CLI and the MCP
/// server so both gate identically.
#[derive(Debug, Clone, Default)]
pub struct TimelineQuery {
    /// Restrict to one repo (registry name).
    pub repo: Option<String>,
    /// Substring (case-insensitive) the author email must contain; `None` scopes
    /// to [`Self::me`].
    pub author: Option<String>,
    /// Who "I" am, for the default author scope.
    pub me: MeConfig,
    /// Outbound gate (#336): when set, only `Public` repos pass by default —
    /// the stricter `is_restricted` rule for narration/export that leaves the
    /// machine. When clear (the local `timeline` view), only proprietary and
    /// unregistered repos are restricted; private repos show.
    pub public_only: bool,
    /// Include restricted repos despite the gate (explicit opt-in).
    pub include_restricted: bool,
    /// Cap how many rows are returned (most recent kept).
    pub limit: usize,
}

/// A filtered timeline view: the kept rows (newest first, capped) plus the
/// transparency counts the caller echoes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    pub rows: Vec<AtlasTimelineRow>,
    /// Matched the filters before the `limit` cap.
    pub matched: usize,
    /// Hidden because their repo is proprietary or no longer registered.
    pub excluded_restricted: usize,
    /// Hidden because the author didn't match.
    pub excluded_author: usize,
}

/// Filter store-produced timeline rows by repo, visibility, and author, newest
/// first, capped at `q.limit` (#333). Default-deny: a commit whose repo is
/// proprietary OR has no registry entry (removed/renamed/stale) is excluded
/// unless `include_restricted`. Visibility is resolved from the registry
/// (authoritative, mutable via `repo set-visibility`).
pub fn filter_timeline(
    rows: Vec<AtlasTimelineRow>,
    registry: &crate::registry::Registry,
    q: &TimelineQuery,
) -> Timeline {
    let mut excluded_restricted = 0;
    let mut excluded_author = 0;
    let mut kept: Vec<AtlasTimelineRow> = Vec::new();
    for row in rows {
        if q.repo.as_ref().is_some_and(|name| &row.repo != name) {
            continue;
        }
        let restricted = match registry.get(&row.repo).map(|e| e.visibility) {
            // Unregistered (removed/renamed/stale) is always restricted.
            None => true,
            // Outbound (story/export): anything but Public.
            Some(v) if q.public_only => v.is_restricted(),
            // Local view: only proprietary; private repos show.
            Some(crate::registry::Visibility::Proprietary) => true,
            Some(_) => false,
        };
        if restricted && !q.include_restricted {
            excluded_restricted += 1;
            continue;
        }
        if !author_matches(&q.author, &q.me, &row.commit.author_email) {
            excluded_author += 1;
            continue;
        }
        kept.push(row);
    }
    // Newest first, then cap.
    kept.reverse();
    let matched = kept.len();
    kept.truncate(q.limit);
    Timeline {
        rows: kept,
        matched,
        excluded_restricted,
        excluded_author,
    }
}

/// Whether a commit's author passes the filter: an explicit substring
/// (case-insensitive) wins; otherwise scope to me, falling back to everyone only
/// when no `[me]` and no git email could be resolved.
pub fn author_matches(explicit: &Option<String>, me: &MeConfig, email: &str) -> bool {
    match explicit {
        Some(sub) => email
            .to_ascii_lowercase()
            .contains(&sub.to_ascii_lowercase()),
        None if me.is_set() => me.matches(email),
        None => true,
    }
}

/// A human label for the author scope of a timeline query.
pub fn author_scope_label(q: &TimelineQuery) -> String {
    match &q.author {
        Some(a) => format!("author ~ {a}"),
        None if q.me.is_set() => format!("mine ({})", q.me.display().unwrap_or_default()),
        None => "anyone (no [me] configured)".to_string(),
    }
}

/// The structured timeline value emitted by the CLI (`--json`/`--yaml`) and the
/// MCP timeline tool, so both render identically.
pub fn timeline_value(tl: &Timeline, window: &str, author_scope: &str) -> serde_json::Value {
    let commits: Vec<_> = tl
        .rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "date": format_date(r.commit.committed_at),
                "committed_at": r.commit.committed_at,
                "repo": r.repo,
                "author": r.commit.author_email,
                "subject": r.commit.subject,
                "touched": r.touched,
                "hash": r.commit.hash,
            })
        })
        .collect();
    serde_json::json!({
        "window": window,
        "author_scope": author_scope,
        "shown": tl.rows.len(),
        "matched": tl.matched,
        "excluded": { "restricted": tl.excluded_restricted, "author": tl.excluded_author },
        "commits": commits,
    })
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
    #[serde(default)]
    pub waka: WakaConfig,
    #[serde(default)]
    pub repos: ReposConfig,
}

/// `[repos]` — classify repos by name/forge-org glob so a whole organization or
/// naming convention is treated as work/private without tagging each repo. The
/// patterns only ever *raise* restrictiveness (a `Public` repo can become
/// `Proprietary`, never the reverse); they gate the atlas's public-only and
/// default-deny output paths (#332/#336).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReposConfig {
    /// Globs (`*`/`?`, case-insensitive) matched against the repo name and each
    /// forge id (`host/owner/repo`); a match marks the repo `Proprietary`, e.g.
    /// `*acme*` or `*/acme-corp/*`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proprietary: Vec<String>,
    /// Same matching, marking a repo at least `Private`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub private: Vec<String>,
    /// Allowlist that **wins** over `proprietary`/`private`: a match pins the repo
    /// `Public`, so a public repo caught by a broad work-org glob stays public.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public: Vec<String>,
}

impl ReposConfig {
    /// Whether any classification pattern is configured.
    pub fn is_set(&self) -> bool {
        !self.proprietary.is_empty() || !self.private.is_empty() || !self.public.is_empty()
    }
}

/// `[waka]` — optional WakaTime time-tracking integration (#486). Pulling
/// summaries is an opt-in, off-machine network fetch (the key is read from
/// `WAKATIME_API_KEY`, never stored); this only configures how the data maps in.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WakaConfig {
    /// Map a WakaTime `project` name to a registry repo name, for the few cases
    /// where they differ. An unmapped project keeps its WakaTime name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, String>,
    /// Override the API base URL (default `https://wakatime.com/api/v1`), e.g. for
    /// a self-hosted Wakapi instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// One aggregated WakaTime datum (#486): the coding `seconds` spent on a given
/// `date` (YYYY-MM-DD) for one value of one `dimension`. Dimensions are the
/// marginal breakdowns WakaTime reports per day — `project`, `language`, `editor`,
/// `os`, `machine`, `category` — plus `total` (the day's grand total, `name` ="").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakaStat {
    pub date: String,
    pub dimension: String,
    pub name: String,
    pub seconds: i64,
}

/// `[me]` — who "I" am, so `atlas sync` can keep only my own commits by default
/// and roll every raw author of mine up to one `Identity` (#331). An address is
/// mine if it is listed in `emails`, sits at one of my owned `domains`, or matches
/// one of my `patterns` (a glob over the whole address, e.g. `me+*@gmail.com` for
/// plus-tag aliases or `*@*.example.com` for subdomains). Seeded best-effort from
/// `git config user.email` and the registry contributors; user-curated.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MeConfig {
    /// Exact addresses that are mine (matched case-insensitively).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<String>,
    /// Domains I own; any address at one of them is mine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    /// Glob patterns matched against the whole address (`*` = any run, `?` = any
    /// one char), e.g. `me+*@gmail.com` or `*@*.example.com`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    /// Bot author identities (glob patterns over the author address) whose commits
    /// count as mine — e.g. `*copilot*@users.noreply.github.com`, `claude[bot]@*`.
    /// Bots are *supplementary* (they don't make `is_set` true on their own) and are
    /// only honoured in non-proprietary repos; a commit that credits one of my
    /// `emails`/`domains`/`patterns` via `Co-authored-by` is mine regardless.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bots: Vec<String>,
}

impl MeConfig {
    /// Whether any identity is configured.
    pub fn is_set(&self) -> bool {
        !self.emails.is_empty() || !self.domains.is_empty() || !self.patterns.is_empty()
    }

    /// Whether `email` resolves to me: an exact address match (case-insensitive,
    /// and provider-aware — see [`canonical_email`], so `m.mayer+x@gmail.com` and
    /// `mmayer@gmail.com` are the same person), any address at one of my owned
    /// domains, or a match of one of my glob patterns.
    pub fn matches(&self, email: &str) -> bool {
        let email = email.trim().to_ascii_lowercase();
        let canon = canonical_email(&email);
        if self
            .emails
            .iter()
            .any(|m| canonical_email(m.trim()) == canon)
        {
            return true;
        }
        if let Some((_, domain)) = email.rsplit_once('@')
            && self.domains.iter().any(|d| d.eq_ignore_ascii_case(domain))
        {
            return true;
        }
        self.patterns
            .iter()
            .any(|p| glob_match_ci(p.trim(), &email))
    }

    /// Whether `email` is one of my configured bot identities (`bots` globs over the
    /// author address). Caller gates this on repo visibility (non-proprietary only).
    pub fn bot_matches(&self, email: &str) -> bool {
        let email = email.trim().to_ascii_lowercase();
        self.bots.iter().any(|p| glob_match_ci(p.trim(), &email))
    }

    /// A display address for my unified identity: the first configured email, the
    /// first pattern, or `me@<first domain>` when only domains are known.
    pub fn display(&self) -> Option<String> {
        self.emails
            .first()
            .or_else(|| self.patterns.first())
            .cloned()
            .or_else(|| self.domains.first().map(|d| format!("me@{d}")))
    }

    /// A one-line summary of *every* identity that counts as mine (all `emails`,
    /// owned `domains` shown as `@domain`, and `patterns`), so the matched scope is
    /// transparent rather than reduced to a single representative [`display`](Self::display)
    /// address. `None` when nothing is configured.
    pub fn summary(&self) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        let parts: Vec<String> = self
            .emails
            .iter()
            .cloned()
            .chain(self.domains.iter().map(|d| format!("@{d}")))
            .chain(self.patterns.iter().cloned())
            .chain(self.bots.iter().map(|b| format!("bot:{b}")))
            .collect();
        Some(parts.join(", "))
    }
}

/// How a provider folds the local part of an address: ignore everything from a
/// `+` onward (sub-addressing tags), and/or ignore `.` separators — so that
/// several written addresses deliver to one mailbox and so denote one person.
struct ProviderFolding {
    plus_tags: bool,
    dots: bool,
}

/// The folding rules for a mail provider's domain, **only** where the behaviour is
/// documented and certain. Conservative by design: an unlisted domain folds
/// nothing (a `+` or `.` there may be significant), so we never merge two distinct
/// people. Gmail is the sole provider that also ignores dots.
fn provider_folding(domain: &str) -> Option<ProviderFolding> {
    match domain {
        // Google: sub-addressing with `+`, and dots in the local part are ignored.
        "gmail.com" | "googlemail.com" => Some(ProviderFolding {
            plus_tags: true,
            dots: true,
        }),
        // Microsoft consumer, Apple iCloud, Fastmail, Proton: documented `+`
        // sub-addressing; dots are significant on all of them.
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "icloud.com" | "me.com"
        | "mac.com" | "fastmail.com" | "fastmail.fm" | "proton.me" | "protonmail.com" | "pm.me" => {
            Some(ProviderFolding {
                plus_tags: true,
                dots: false,
            })
        }
        _ => None,
    }
}

/// Canonicalise an address to the mailbox it actually delivers to, for identity
/// matching. Always lowercased and trimmed; additionally, for the handful of
/// providers whose folding is certain ([`provider_folding`]), drops a `+tag`
/// sub-address and (Gmail only) the dots in the local part — so `M.Mayer+ci@gmail.com`
/// and `mmayer@gmail.com` canonicalise alike. Addresses on any other domain (and
/// anything without a single `@`) are returned lowercased but otherwise untouched,
/// so two genuinely different people are never merged.
pub fn canonical_email(email: &str) -> String {
    let email = email.trim().to_ascii_lowercase();
    let Some((local, domain)) = email.rsplit_once('@') else {
        return email;
    };
    let Some(folding) = provider_folding(domain) else {
        return email;
    };
    let mut local = local;
    if folding.plus_tags
        && let Some((base, _tag)) = local.split_once('+')
    {
        local = base;
    }
    let local = if folding.dots {
        local.replace('.', "")
    } else {
        local.to_string()
    };
    // A pathological all-tag/all-dot local part would empty out; keep the address
    // well-formed by falling back rather than producing `@domain`.
    if local.is_empty() {
        return email;
    }
    format!("{local}@{domain}")
}

/// Case-insensitive glob match (`*` = any run incl. empty, `?` = any one char),
/// with linear-time star backtracking. Used for `[me].patterns` and `[repos]`.
pub(crate) fn glob_match_ci(pattern: &str, text: &str) -> bool {
    let pat = pattern.to_ascii_lowercase().into_bytes();
    let txt = text.as_bytes(); // `text` is already lowercased by the caller
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < txt.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
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

/// Repos `atlas sync --github` should skip: ones found to have **zero commits by
/// any `[me]` email**, recorded so a later scan doesn't re-clone/re-walk them
/// (#nnn). Keyed by canonical forge source (`host/owner/repo`). Persisted as
/// `~/.glyphtrail/atlas/ignored.toml` — human-inspectable, and hand-editable to
/// force an ignore. Self-correcting: finding any `me` commit removes a repo here.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IgnoredRepos {
    #[serde(default)]
    pub repos: BTreeSet<String>,
}

impl IgnoredRepos {
    /// Load `ignored.toml` from `atlas_dir`; empty when absent.
    pub fn load(atlas_dir: &Path) -> crate::Result<IgnoredRepos> {
        let path = atlas_dir.join("ignored.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|source| crate::error::CoreError::ConfigParse {
                    path,
                    source: Box::new(source),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IgnoredRepos::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist to `ignored.toml` under `atlas_dir`, atomically (temp + rename),
    /// mirroring [`AtlasHeads::save`].
    pub fn save(&self, atlas_dir: &Path) -> crate::Result<()> {
        let path = atlas_dir.join("ignored.toml");
        let toml =
            toml::to_string_pretty(self).map_err(|e| crate::error::CoreError::ConfigInvalid {
                path: path.clone(),
                message: e.to_string(),
            })?;
        let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, toml)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn contains(&self, source: &str) -> bool {
        self.repos.contains(source)
    }

    /// Mark `source` ignored. Returns whether it was newly added.
    pub fn insert(&mut self, source: &str) -> bool {
        self.repos.insert(source.to_string())
    }

    /// Un-ignore `source`. Returns whether it had been ignored.
    pub fn remove(&mut self, source: &str) -> bool {
        self.repos.remove(source)
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
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

    /// The window as `none` or `earliest..latest` (an open side prints empty).
    pub fn label(&self) -> String {
        match (&self.earliest, &self.latest) {
            (None, None) => "none".to_string(),
            (earliest, latest) => format!(
                "{}..{}",
                earliest.as_deref().unwrap_or(""),
                latest.as_deref().unwrap_or("")
            ),
        }
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
    fn paths_digest_is_bounded_and_meaningful() {
        // A sparse-message commit adding hundreds of images: the digest captures the
        // directory + image extension regardless of file count, and stays tiny.
        let mut paths: Vec<String> = (0..500).map(|i| format!("data/IMG_{i:04}.jpg")).collect();
        paths.push("README.md".to_string());
        let d = paths_digest(&paths);
        check!(d.split(' ').count() <= 20); // bounded
        check!(d.contains("data")); // the dominant directory
        check!(d.contains("jpg")); // the dominant extension
        // Frequency ordering: the 500-file extension comes before the lone `md`.
        let jpg = d.find("jpg").unwrap();
        let md = d.find("md").unwrap();
        check!(jpg < md);
    }

    #[test]
    fn paths_digest_handles_dotfiles_and_empty() {
        check!(paths_digest(&[]).is_empty());
        // `.gitignore` has no real extension (empty stem), so it adds no ext token.
        let d = paths_digest(&[".gitignore".to_string(), "src/main.rs".to_string()]);
        check!(d.contains("src") && d.contains("rs"));
        check!(!d.split(' ').any(|t| t == "gitignore"));
    }

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
            ..Default::default()
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
    fn canonical_email_folds_only_known_providers() {
        // Gmail: drops +tags AND dots, case-insensitively.
        check!(canonical_email("M.Mayer+ci@Gmail.com") == "mmayer@gmail.com");
        check!(canonical_email("mmayer@gmail.com") == "mmayer@gmail.com");
        check!(canonical_email("m.mayer@googlemail.com") == "mmayer@googlemail.com");
        // Plus-only providers: drop the tag, keep dots (dots are significant there).
        check!(canonical_email("john.doe+news@outlook.com") == "john.doe@outlook.com");
        check!(canonical_email("a.b+x@icloud.com") == "a.b@icloud.com");
        check!(canonical_email("a+x@proton.me") == "a@proton.me");
        // Unknown domain: never folded — a `+`/`.` there may be significant.
        check!(canonical_email("first.last+tag@example.com") == "first.last+tag@example.com");
        // Degenerate inputs: lowercased/trimmed, otherwise untouched.
        check!(canonical_email("  Plain  ") == "plain");
        check!(canonical_email("+only@gmail.com") == "+only@gmail.com"); // empties out → keep
    }

    #[test]
    fn me_matches_provider_folded_aliases() {
        let me = MeConfig {
            emails: vec!["mmayer@gmail.com".into()],
            ..Default::default()
        };
        // Dot/plus variants of the same Gmail mailbox are me.
        check!(me.matches("m.mayer@gmail.com"));
        check!(me.matches("M.Mayer+work@gmail.com"));
        check!(me.matches("mmayer+ci@gmail.com"));
        // A genuinely different Gmail local part is not.
        check!(!me.matches("m.mayer.jr@gmail.com")); // -> mmayerjr, distinct
        // The same fold the other way: config carries the dotted form.
        let me2 = MeConfig {
            emails: vec!["m.mayer@gmail.com".into()],
            ..Default::default()
        };
        check!(me2.matches("mmayer@gmail.com"));
    }

    #[test]
    fn me_summary_lists_every_identity() {
        let me = MeConfig {
            emails: vec!["a@x.com".into(), "b@y.com".into()],
            domains: vec!["mine.dev".into()],
            patterns: vec!["me+*@gmail.com".into()],
            bots: vec!["claude[bot]@*".into()],
        };
        // Every group is named, so the matched scope is transparent — not reduced to
        // the single representative `display` address.
        check!(
            me.summary().as_deref()
                == Some("a@x.com, b@y.com, @mine.dev, me+*@gmail.com, bot:claude[bot]@*")
        );
        check!(me.display().as_deref() == Some("a@x.com")); // still just the first
        // Nothing configured → no summary.
        check!(MeConfig::default().summary() == None);
        // Bots alone don't make an identity (sync still can't tell who you are).
        let bots_only = MeConfig {
            bots: vec!["*@bots.dev".into()],
            ..Default::default()
        };
        check!(!bots_only.is_set());
        check!(bots_only.summary() == None);
    }

    #[test]
    fn me_bot_matches_globs_the_author_address() {
        let me = MeConfig {
            bots: vec![
                "*copilot*@users.noreply.github.com".into(),
                "claude[bot]@*".into(),
            ],
            ..Default::default()
        };
        check!(me.bot_matches("Copilot@users.noreply.github.com")); // case-insensitive
        check!(me.bot_matches("198982749+copilot@users.noreply.github.com"));
        check!(me.bot_matches("claude[bot]@anthropic.com"));
        check!(!me.bot_matches("me@example.com")); // a human isn't a bot
        check!(!me.bot_matches("dependabot[bot]@users.noreply.github.com"));
        // bot_matches is independent of matches (different list).
        check!(!me.matches("copilot@users.noreply.github.com"));
    }

    #[test]
    fn me_matches_glob_patterns() {
        let me = MeConfig {
            patterns: vec!["me+*@gmail.com".into(), "*@*.example.com".into()],
            ..Default::default()
        };
        // Plus-tag aliases on a shared provider.
        check!(me.matches("me+work@gmail.com"));
        check!(me.matches("ME+Foo@Gmail.com")); // case-insensitive
        check!(me.matches("me+@gmail.com")); // `*` matches empty
        check!(!me.matches("me@gmail.com")); // no `+` → no match (don't claim all gmail)
        check!(!me.matches("someoneelse+x@gmail.com"));
        // Subdomain wildcard.
        check!(me.matches("ada@eng.example.com"));
        check!(!me.matches("ada@example.com")); // `*.example.com` needs a subdomain
        check!(me.is_set());
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
    fn author_matches_explicit_substring_then_me_then_everyone() {
        let me = MeConfig {
            emails: vec!["ada@x.dev".into()],
            domains: vec![],
            ..Default::default()
        };
        // Explicit substring (case-insensitive) wins, ignoring me.
        check!(author_matches(&Some("EVE".into()), &me, "eve@evil.dev"));
        check!(!author_matches(&Some("eve".into()), &me, "ada@x.dev"));
        // No author filter: scope to me.
        check!(author_matches(&None, &me, "ada@x.dev"));
        check!(!author_matches(&None, &me, "eve@evil.dev"));
        // No author filter and no [me]: everyone passes.
        check!(author_matches(&None, &MeConfig::default(), "anyone@here"));
    }

    #[test]
    fn filter_timeline_default_denies_proprietary_and_unregistered() {
        use crate::registry::{Registry, RegistryEntry, Visibility};
        let row = |repo: &str, email: &str, at: i64| AtlasTimelineRow {
            commit: CommitMeta {
                node_id: NodeId(format!("{repo}-{at}")),
                hash: "h".into(),
                author_email: email.into(),
                committed_at: at,
                subject: "s".into(),
                in_bounds: true,
            },
            repo: repo.into(),
            touched: 1,
        };
        let entry = |name: &str, v: Visibility| RegistryEntry {
            name: name.into(),
            root: format!("/{name}").into(),
            alt_roots: vec![],
            missing_since: None,
            ids: vec![],
            contributors: vec![],
            identity: None,
            visibility: v,
        };
        let mut registry = Registry::default();
        registry.repos.push(entry("pub", Visibility::Public));
        registry.repos.push(entry("priv", Visibility::Private));
        registry.repos.push(entry("prop", Visibility::Proprietary));
        // As the store yields them: ascending by committed_at (oldest first).
        let rows = vec![
            row("ghost", "ada@x.dev", 0), // not in the registry
            row("prop", "ada@x.dev", 1),
            row("priv", "ada@x.dev", 2),
            row("pub", "ada@x.dev", 3),
        ];
        let q = TimelineQuery {
            me: MeConfig {
                emails: vec!["ada@x.dev".into()],
                domains: vec![],
                ..Default::default()
            },
            limit: 10,
            ..Default::default()
        };
        // Default-deny hides proprietary + unregistered; public + private show.
        let tl = filter_timeline(rows.clone(), &registry, &q);
        check!(tl.rows.len() == 2);
        check!(tl.excluded_restricted == 2);
        check!(tl.rows[0].repo == "pub"); // newest first
        check!(tl.rows[1].repo == "priv");
        // Opt-in includes them.
        let tl = filter_timeline(
            rows.clone(),
            &registry,
            &TimelineQuery {
                include_restricted: true,
                ..q.clone()
            },
        );
        check!(tl.rows.len() == 4 && tl.excluded_restricted == 0);
        // Outbound (public_only): private is restricted too — only `pub` shows.
        let tl = filter_timeline(
            rows,
            &registry,
            &TimelineQuery {
                public_only: true,
                ..q
            },
        );
        check!(tl.rows.len() == 1 && tl.rows[0].repo == "pub");
        check!(tl.excluded_restricted == 3); // priv + prop + ghost
    }

    #[test]
    fn derive_topics_pulls_keywords_dirs_and_languages() {
        let topics = derive_topics(
            "Add parser recovery for nested blocks",
            &[
                "src/parser/recover.rs".into(),
                "src/ui/io/buf.rs".into(),
                "src/lib.rs".into(),
            ],
        );
        // Significant subject keywords kept; "add"/"for" dropped as stopwords.
        check!(topics.contains(&"parser".to_string()));
        check!(topics.contains(&"recovery".to_string()));
        check!(topics.contains(&"nested".to_string()));
        check!(!topics.contains(&"add".to_string()));
        check!(!topics.contains(&"for".to_string()));
        // A meaningful directory becomes a topic; generic "src" does not.
        check!(!topics.contains(&"src".to_string()));
        // Short area directories (2 chars) survive as topics.
        check!(topics.contains(&"ui".to_string()));
        check!(topics.contains(&"io".to_string()));
        // Language inferred from the extension.
        check!(topics.contains(&"rust".to_string()));
        // Output is sorted + de-duplicated.
        let mut sorted = topics.clone();
        sorted.sort();
        check!(topics == sorted);
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

    #[test]
    fn ignored_repos_round_trip() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gt-atlas-ignored-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        check!(IgnoredRepos::load(&dir).unwrap().is_empty());

        let mut ig = IgnoredRepos::default();
        check!(ig.insert("github.com/acme/widgets"));
        check!(!ig.insert("github.com/acme/widgets")); // already present
        ig.insert("github.com/acme/gizmos");
        ig.save(&dir).unwrap();

        let loaded = IgnoredRepos::load(&dir).unwrap();
        check!(loaded.contains("github.com/acme/widgets"));
        check!(loaded.contains("github.com/acme/gizmos"));
        check!(!loaded.contains("github.com/acme/other"));

        // Un-ignore (the "found a me-commit" path) drops it.
        let mut loaded = loaded;
        check!(loaded.remove("github.com/acme/widgets"));
        loaded.save(&dir).unwrap();
        check!(
            !IgnoredRepos::load(&dir)
                .unwrap()
                .contains("github.com/acme/widgets")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
