//! Atlas (#329/#330): the opt-in, local-only global archaeology index. This
//! module is the lifecycle surface — create the store, report its state and the
//! active limits, print its path. Ingestion (#331) and queries (#333) build on
//! it. Atlas writes only under `~/.glyphtrail/atlas/`; nothing here touches the
//! network or exports.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use glyphtrail_core::{
    AtlasConfig, AtlasHeads, CodeGraph, CommitMeta, Confidence, EdgeKind, MeConfig, Node, NodeId,
    NodeKind, Registry, RegistryEntry, TimelineQuery, Window, author_scope_label,
    default_atlas_path, default_registry_path, filter_timeline, format_date, scrub_secrets,
    timeline_value,
};
use glyphtrail_store::{GraphStore, LadybugStore};

use super::query::{Emit, print_value};

#[derive(Subcommand)]
pub enum AtlasCmd {
    /// Create the atlas store (opt-in). Idempotent.
    Init,
    /// Report whether atlas is enabled, its path, counts, and the active limits.
    Status,
    /// Print the atlas store's LadybugDB directory.
    Path,
    /// Ingest git history into the atlas (mine-only by default, incremental).
    Sync(SyncArgs),
    /// List commits chronologically across repos (mine by default).
    Timeline(TimelineArgs),
    /// List the derived topics and how many commits each tags.
    Topics(TopicsArgs),
    /// Narrate the evolution of your work across repos via an LLM (public-only).
    Story(StoryArgs),
    /// Export the gated atlas timeline as structured data (public-only).
    Export(ExportArgs),
    /// Serve the atlas over MCP (stdio): timeline, status, and the repo+file bridge.
    Mcp,
}

#[derive(Args)]
pub struct StoryArgs {
    /// LLM provider.
    #[arg(long, value_enum, default_value_t)]
    pub provider: crate::commands::llm::Provider,
    /// Model id (defaults to a sensible per-provider model).
    #[arg(long)]
    pub model: Option<String>,
    /// Override the API base URL (for OpenAI-compatible gateways).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Earliest commit date (YYYY-MM-DD); overrides the config window.
    #[arg(long)]
    pub since: Option<String>,
    /// Latest commit date (YYYY-MM-DD); overrides the config window.
    #[arg(long)]
    pub until: Option<String>,
    /// Most recent commits to narrate.
    #[arg(long, default_value_t = 300)]
    pub max_commits: usize,
    /// Include restricted repos — private, proprietary, or unregistered (excluded
    /// by default, since this output leaves the machine).
    #[arg(long)]
    pub include_restricted: bool,
    /// Output file for the narrative.
    #[arg(long, default_value = "ATLAS.md")]
    pub output: PathBuf,
    /// Write the composed prompt instead of calling the LLM (no network/keys).
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct ExportArgs {
    /// Earliest commit date (YYYY-MM-DD); overrides the config window.
    #[arg(long)]
    pub since: Option<String>,
    /// Latest commit date (YYYY-MM-DD); overrides the config window.
    #[arg(long)]
    pub until: Option<String>,
    /// Most recent commits to export.
    #[arg(long, default_value_t = 1000)]
    pub limit: usize,
    /// Include restricted repos — private, proprietary, or unregistered (excluded
    /// by default).
    #[arg(long)]
    pub include_restricted: bool,
    /// Write to a file instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Emit JSON instead of YAML.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct TopicsArgs {
    /// Cap how many topics are shown (most-tagged first).
    #[arg(long, default_value_t = 40)]
    pub limit: usize,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
    /// Emit YAML.
    #[arg(long)]
    pub yaml: bool,
}

#[derive(Args)]
pub struct TimelineArgs {
    /// Only commits in this repo (registry name).
    #[arg(long)]
    pub repo: Option<String>,
    /// Only commits whose author email contains this (default: me).
    #[arg(long)]
    pub author: Option<String>,
    /// Only commits tagged with this topic (see `atlas topics`).
    #[arg(long)]
    pub topic: Option<String>,
    /// Earliest commit date (YYYY-MM-DD); overrides the config window.
    #[arg(long)]
    pub since: Option<String>,
    /// Latest commit date (YYYY-MM-DD); overrides the config window.
    #[arg(long)]
    pub until: Option<String>,
    /// Include commits from proprietary repos (excluded by default).
    #[arg(long)]
    pub include_proprietary: bool,
    /// Cap how many commits are shown (most recent kept).
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
    /// Emit YAML.
    #[arg(long)]
    pub yaml: bool,
}

#[derive(Args)]
pub struct SyncArgs {
    /// Sync only this registered repo (default: every registered repo).
    #[arg(long)]
    pub repo: Option<String>,
    /// Re-walk full history, ignoring the saved per-repo HEAD watermark.
    #[arg(long)]
    pub full: bool,
    /// Override the window's earliest bound for this run (YYYY-MM-DD).
    #[arg(long)]
    pub since: Option<String>,
    /// Override the window's latest bound for this run (YYYY-MM-DD).
    #[arg(long)]
    pub until: Option<String>,
    /// Ingest commits by everyone, not just my own.
    #[arg(long)]
    pub everyone: bool,
}

/// `~/.glyphtrail/atlas/`, or an error when no home directory is set.
fn atlas_dir() -> Result<PathBuf> {
    default_atlas_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

/// The LadybugDB store directory inside the atlas dir.
fn ladybug_dir(atlas: &Path) -> PathBuf {
    atlas.join("ladybug")
}

pub fn run(cmd: AtlasCmd) -> Result<()> {
    let dir = atlas_dir()?;
    match cmd {
        AtlasCmd::Init => {
            std::fs::create_dir_all(&dir)?;
            // Open once to stamp the schema (creates the ladybug dir + tables).
            LadybugStore::open(&ladybug_dir(&dir))?;
            println!("atlas initialized at {}", dir.display());
        }
        AtlasCmd::Path => println!("{}", ladybug_dir(&dir).display()),
        AtlasCmd::Status => status(&dir)?,
        AtlasCmd::Sync(args) => sync(&dir, args)?,
        AtlasCmd::Timeline(args) => timeline(&dir, args)?,
        AtlasCmd::Topics(args) => topics(&dir, args)?,
        AtlasCmd::Story(args) => story(&dir, args)?,
        AtlasCmd::Export(args) => export(&dir, args)?,
        AtlasCmd::Mcp => glyphtrail_mcp::serve_atlas_stdio(dir)?,
    }
    Ok(())
}

const ATLAS_SYSTEM: &str = "You are a technical writer narrating the evolution of one \
developer's work across their projects, from a chronological commit log spanning several \
repositories. Use only the provided facts — do not invent commits, features, dates, or people. \
Write engaging, accurate GitHub-flavored Markdown: the arc of what they worked on over time, \
recurring themes, and how their focus shifted between projects. Group related commits into themes \
rather than listing them verbatim.";

/// Gather the **outbound** (public-only) gated timeline shared by `atlas story`
/// and `atlas export` (#336): private/proprietary/unregistered repos are
/// excluded unless `include_restricted`, since this output leaves the machine.
/// Returns the filtered timeline, the window label, and the author-scope label.
fn outbound_timeline(
    dir: &Path,
    since: Option<String>,
    until: Option<String>,
    include_restricted: bool,
    limit: usize,
) -> Result<(glyphtrail_core::Timeline, String, String)> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let cfg = AtlasConfig::load(dir)?;
    let window = Window {
        earliest: since.or_else(|| cfg.window.earliest.clone()),
        latest: until.or_else(|| cfg.window.latest.clone()),
    };
    let (since, until) = window
        .epoch_bounds()
        .map_err(|d| anyhow!("invalid date: {d}"))?;
    let registry = match default_registry_path() {
        Some(p) => Registry::load(&p)?,
        None => Registry::default(),
    };
    let query = TimelineQuery {
        repo: None,
        author: None,
        me: resolve_me(&cfg.me),
        public_only: true,
        include_restricted,
        limit,
    };
    let store = LadybugStore::open(&lb)?;
    let rows = store.atlas_timeline(since, until, None)?;
    let tl = filter_timeline(rows, &registry, &query);
    Ok((tl, window.label(), author_scope_label(&query)))
}

/// `glyphtrail atlas story` — an LLM narrative of the evolution of your work
/// across repos, over the public-only gated timeline (#336).
fn story(dir: &Path, args: StoryArgs) -> Result<()> {
    let (tl, window, _scope) = outbound_timeline(
        dir,
        args.since,
        args.until,
        args.include_restricted,
        args.max_commits,
    )?;
    if tl.rows.is_empty() {
        bail!(
            "no public commits to narrate (run `glyphtrail atlas sync`, or pass \
             --include-restricted to include private/proprietary repos)"
        );
    }
    eprintln!(
        "atlas story: {} commits; {} restricted hidden",
        tl.rows.len(),
        tl.excluded_restricted
    );
    let prompt = atlas_story_prompt(&tl, &window);

    if args.dry_run {
        std::fs::write(
            &args.output,
            format!("# SYSTEM\n{ATLAS_SYSTEM}\n\n# USER\n{prompt}\n"),
        )
        .with_context(|| format!("cannot write {}", args.output.display()))?;
        println!(
            "wrote {} ({} commits; dry run, no LLM called)",
            args.output.display(),
            tl.rows.len()
        );
        return Ok(());
    }

    let llm = crate::commands::llm::Llm::new(args.provider, args.model, args.base_url)?;
    let md = llm
        .complete(ATLAS_SYSTEM, &prompt)
        .context("generating atlas story")?;
    std::fs::write(&args.output, md)
        .with_context(|| format!("cannot write {}", args.output.display()))?;
    println!("wrote {}", args.output.display());
    Ok(())
}

/// Compose the narration prompt from the gated timeline: a facts header plus the
/// commit log oldest-first (so the model reads a forward arc).
fn atlas_story_prompt(tl: &glyphtrail_core::Timeline, window: &str) -> String {
    use std::collections::BTreeSet;
    let repos: BTreeSet<&str> = tl.rows.iter().map(|r| r.repo.as_str()).collect();
    let log = tl
        .rows
        .iter()
        .rev() // rows are newest-first; narrate oldest-first
        .map(|r| {
            format!(
                "- {} [{}] {}",
                format_date(r.commit.committed_at),
                r.repo,
                r.commit.subject
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Window: {window}\nRepositories ({}): {}\nCommits: {}\n\n\
         Commit log (oldest first):\n{log}\n\n\
         Write a narrative of how my work evolved over this window — the arc across \
         these projects, recurring themes, and how my focus shifted between them.",
        repos.len(),
        repos.into_iter().collect::<Vec<_>>().join(", "),
        tl.rows.len(),
    )
}

/// `glyphtrail atlas export` — the gated timeline as structured JSON/YAML, to
/// stdout or a file (#336). Public-only by default.
fn export(dir: &Path, args: ExportArgs) -> Result<()> {
    let (tl, window, scope) = outbound_timeline(
        dir,
        args.since,
        args.until,
        args.include_restricted,
        args.limit,
    )?;
    eprintln!(
        "atlas export: {} commits; {} restricted hidden",
        tl.rows.len(),
        tl.excluded_restricted
    );
    let value = timeline_value(&tl, &window, &scope);
    let text = if args.json {
        serde_json::to_string_pretty(&value)?
    } else {
        serde_norway::to_string(&value)?
    };
    match &args.output {
        Some(path) => {
            std::fs::write(path, &text)
                .with_context(|| format!("cannot write {}", path.display()))?;
            println!("wrote {} ({} commits)", path.display(), tl.rows.len());
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// `glyphtrail atlas topics` — the derived topics and their commit counts.
fn topics(dir: &Path, args: TopicsArgs) -> Result<()> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let store = LadybugStore::open(&lb)?;
    let mut topics = store.atlas_topics()?;
    topics.truncate(args.limit);

    let emit = Emit::from_flags(args.json, args.yaml);
    if emit == Emit::Text {
        if topics.is_empty() {
            println!("no topics yet (run `glyphtrail atlas sync`)");
        }
        for (name, count) in &topics {
            println!("  {count:>5}  {name}");
        }
    } else {
        let value = serde_json::Value::Array(
            topics
                .iter()
                .map(|(name, count)| serde_json::json!({ "topic": name, "commits": count }))
                .collect(),
        );
        print_value(&value, emit)?;
    }
    Ok(())
}

/// Report state + the active limits. Establishes the convention every later
/// atlas command follows: always echo the gates so nothing is excluded silently.
fn status(dir: &Path) -> Result<()> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        println!("atlas:  disabled (run `glyphtrail atlas init` to enable)");
        println!("would live at: {}", dir.display());
        return Ok(());
    }
    let store = LadybugStore::open(&lb)?;
    let stats = store.stats()?;
    let commits = store.commit_count()?;
    let cfg = AtlasConfig::load(dir)?;

    println!("atlas:   enabled");
    println!("path:    {}", lb.display());
    println!("nodes:   {}", stats.nodes);
    println!("edges:   {}", stats.edges);
    println!("commits: {commits}");
    println!("window:  {}", cfg.window.label());
    Ok(())
}

/// A single commit's git facts, as gathered from `git log` (#331).
struct RawCommit {
    hash: String,
    committed_at: i64,
    author_name: String,
    author_email: String,
    subject: String,
    files: Vec<String>,
}

/// `glyphtrail atlas sync` — walk each registered repo's git history into the
/// atlas. Mine-only by default, incremental from a saved per-repo HEAD, gated by
/// the configured date window. Echoes every active gate before writing.
fn sync(dir: &Path, args: SyncArgs) -> Result<()> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }

    let reg_path = default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))?;
    let registry = Registry::load(&reg_path)?;
    let selected: Vec<&RegistryEntry> = match &args.repo {
        Some(name) => vec![
            registry
                .get(name)
                .ok_or_else(|| anyhow!("no repository named '{name}' in the registry"))?,
        ],
        None => registry.repos.iter().collect(),
    };
    if selected.is_empty() {
        bail!("no repositories registered; use `glyphtrail repo add`");
    }

    let cfg = AtlasConfig::load(dir)?;
    // The persistent config window drives `in_bounds` (and the later re-mark);
    // the CLI flags only widen/narrow *this run's* walk.
    let (bound_since, bound_until) = cfg
        .window
        .epoch_bounds()
        .map_err(|d| anyhow!("invalid window date in atlas.toml: {d}"))?;
    let walk = Window {
        earliest: args.since.clone().or_else(|| cfg.window.earliest.clone()),
        latest: args.until.clone().or_else(|| cfg.window.latest.clone()),
    };
    let (walk_since, walk_until) = walk
        .epoch_bounds()
        .map_err(|d| anyhow!("invalid --since/--until date: {d}"))?;

    let me = resolve_me(&cfg.me);
    if !args.everyone && !me.is_set() {
        bail!(
            "cannot tell who you are: add an [me] section (emails/domains) to \
             {}/atlas.toml, or pass --everyone",
            dir.display()
        );
    }

    println!("atlas sync");
    println!("  window:  {}", cfg.window.label());
    if walk.earliest != cfg.window.earliest || walk.latest != cfg.window.latest {
        println!("  walk:    {} (this run only)", walk.label());
    }
    if args.everyone {
        println!("  authors: everyone");
    } else {
        println!(
            "  authors: mine only ({})",
            me.display().unwrap_or_default()
        );
    }

    let mut heads = AtlasHeads::load(dir)?;
    let mut store = LadybugStore::open(&lb)?;
    let mut total = 0usize;

    for e in &selected {
        let root = e.active_root();
        if !root.exists() {
            println!("  {}: root missing, skipped", e.name);
            continue;
        }
        let Some(head) = git_head(root) else {
            println!("  {}: no commits (not a git repo?), skipped", e.name);
            continue;
        };
        let since_head: Option<String> = if args.full {
            None
        } else {
            heads.get(&e.name).map(str::to_string)
        };
        let commits = match gather_commits(root, since_head.as_deref(), walk_since, walk_until) {
            Ok(c) => c,
            // A rewritten history (the saved HEAD is gone) -> full re-walk. Only
            // on an invalid-range error; a real failure (git missing, broken
            // repo) keeps its context and propagates.
            Err(err) if since_head.is_some() && is_bad_revision(&err) => {
                gather_commits(root, None, walk_since, walk_until)?
            }
            Err(err) => return Err(err),
        };

        let (graph, metas, kept, skipped) =
            build_repo_graph(e, &commits, &me, args.everyone, (bound_since, bound_until));
        store.insert_graph(&graph.nodes, &graph.edges)?;
        store.set_commits(&metas)?;
        heads.set(&e.name, &head);
        total += kept;
        println!(
            "  {}: +{kept} commits, {skipped} skipped (not mine) [{}]",
            e.name,
            e.visibility.as_str()
        );
    }

    heads.save(dir)?;
    // Re-evaluate *every* stored commit against the persistent window, always:
    // narrowing re-marks rows out of bounds, and removing the window (bounds
    // become `None`) restores them to in-bounds (not deletes).
    store.remark_commit_bounds(bound_since, bound_until)?;
    println!("  total:   {total} commits ingested");
    Ok(())
}

/// `glyphtrail atlas timeline` — chronological commits across repos. Default-deny
/// on proprietary repos and scoped to me, unless `--author`/`--include-proprietary`
/// widen it. Echoes the effective window + what was hidden.
fn timeline(dir: &Path, args: TimelineArgs) -> Result<()> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let cfg = AtlasConfig::load(dir)?;
    // Effective window: CLI flags override the config window.
    let window = Window {
        earliest: args.since.clone().or_else(|| cfg.window.earliest.clone()),
        latest: args.until.clone().or_else(|| cfg.window.latest.clone()),
    };
    let (since, until) = window
        .epoch_bounds()
        .map_err(|d| anyhow!("invalid date: {d}"))?;

    let registry = match default_registry_path() {
        Some(p) => Registry::load(&p)?,
        None => Registry::default(),
    };
    let query = TimelineQuery {
        repo: args.repo.clone(),
        author: args.author.clone(),
        me: resolve_me(&cfg.me),
        public_only: false, // local view: private shows; proprietary + unregistered hidden
        include_restricted: args.include_proprietary,
        limit: args.limit,
    };
    let store = LadybugStore::open(&lb)?;
    let rows = store.atlas_timeline(since, until, args.topic.as_deref())?;
    let tl = filter_timeline(rows, &registry, &query);
    let scope = author_scope_label(&query);
    let window_str = window.label();

    let emit = Emit::from_flags(args.json, args.yaml);
    if emit == Emit::Text {
        println!("atlas timeline");
        println!("  window:  {window_str}");
        println!("  author:  {scope}");
        if let Some(r) = &args.repo {
            println!("  repo:    {r}");
        }
        if let Some(t) = &args.topic {
            println!("  topic:   {t}");
        }
        if tl.excluded_restricted > 0 {
            println!(
                "  hidden:  {} restricted (proprietary/unregistered; use --include-proprietary)",
                tl.excluded_restricted
            );
        }
        println!("  showing: {} of {} matched", tl.rows.len(), tl.matched);
        println!();
        for row in &tl.rows {
            println!(
                "  {}  {:<20}  {} ({} file{})",
                format_date(row.commit.committed_at),
                truncate(&row.repo, 20),
                row.commit.subject,
                row.touched,
                if row.touched == 1 { "" } else { "s" },
            );
        }
    } else {
        print_value(&timeline_value(&tl, &window_str, &scope), emit)?;
    }
    Ok(())
}

/// Truncate `s` to `max` chars, appending an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Resolve "me": the configured `[me]`, or a best-effort fallback seeded from
/// `git config user.email` when none is set. Never silently claims others —
/// only the configured/own address(es) match.
fn resolve_me(configured: &MeConfig) -> MeConfig {
    if configured.is_set() {
        return configured.clone();
    }
    let mut me = MeConfig::default();
    if let Some(email) = git_user_email() {
        me.emails.push(email);
    }
    me
}

/// The current `HEAD` commit hash, or `None` when the repo has no commits.
fn git_head(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!head.is_empty()).then_some(head)
}

/// The user's configured git email (`git config --get user.email`), the
/// best-effort seed for an unset `[me]`.
fn git_user_email() -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--get", "user.email"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!email.is_empty()).then_some(email)
}

/// Gather non-merge commits with their touched files. `since_head` (a saved
/// watermark) walks only `<since_head>..HEAD`; `None` walks all of `HEAD`. The
/// window bounds (unix seconds) add `--since=@s`/`--until=@u` so out-of-range
/// commits are never walked — passed as epochs (`@`) so git stays UTC-aligned
/// with `committed_at` / `in_bounds` rather than the local timezone.
fn gather_commits(
    root: &Path,
    since_head: Option<&str>,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Vec<RawCommit>> {
    // A record separator (\x1e) starts each commit; unit separators (\x1f)
    // delimit its fields. The touched files follow on their own lines until the
    // next record separator, so the layout survives any byte in a subject.
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args([
        "log",
        "--no-merges",
        "--name-only",
        "--pretty=format:%x1e%H%x1f%ct%x1f%an%x1f%ae%x1f%s",
    ]);
    if let Some(s) = since {
        cmd.arg(format!("--since=@{s}"));
    }
    if let Some(u) = until {
        cmd.arg(format!("--until=@{u}"));
    }
    if let Some(head) = since_head {
        cmd.arg(format!("{head}..HEAD"));
    }
    let out = cmd.output().map_err(|e| anyhow!("running git log: {e}"))?;
    if !out.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_log(&String::from_utf8_lossy(&out.stdout)))
}

/// Whether a `gather_commits` error is git rejecting the `<head>..HEAD` range
/// (a watermark rewritten out of history), the only case worth a full re-walk;
/// a genuine failure (git missing, broken repo) is not.
fn is_bad_revision(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("bad revision")
        || msg.contains("unknown revision")
        || msg.contains("ambiguous argument")
}

/// Parse the `\x1e`-delimited `git log --name-only` stream into commits.
fn parse_log(text: &str) -> Vec<RawCommit> {
    text.split('\u{1e}')
        .filter(|chunk| !chunk.trim().is_empty())
        .filter_map(|chunk| {
            let (head, rest) = chunk.split_once('\n').unwrap_or((chunk, ""));
            let mut f = head.split('\u{1f}');
            let hash = f.next()?.trim().to_string();
            let committed_at = f.next()?.trim().parse::<i64>().ok()?;
            let author_name = f.next()?.to_string();
            let author_email = f.next()?.to_string();
            let subject = f.next().unwrap_or("").to_string();
            let files = rest
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            Some(RawCommit {
                hash,
                committed_at,
                author_name,
                author_email,
                subject,
                files,
            })
        })
        .collect()
}

/// Whether `t` (unix seconds) is inside the inclusive `[since, until]` bounds
/// (`None` = unbounded).
fn within(t: i64, since: Option<i64>, until: Option<i64>) -> bool {
    since.is_none_or(|s| t >= s) && until.is_none_or(|u| t <= u)
}

/// Add `node` to `g` unless its id was already emitted in this batch.
fn push_node(g: &mut CodeGraph, seen: &mut HashSet<String>, node: Node) {
    if seen.insert(node.id.0.clone()) {
        g.add_node(node);
    }
}

/// A bare atlas node (no span/doc/language) of `kind` with the given id/name.
fn atlas_node(id: NodeId, kind: NodeKind, name: String, qualified_name: String) -> Node {
    Node {
        id,
        kind,
        name,
        qualified_name,
        file: String::new(),
        language: None,
        span: None,
        doc: None,
        signature: None,
    }
}

/// Build the atlas graph fragment + side-table rows for one repo's commits.
/// Returns `(graph, commit rows, kept, skipped)`. Mine-only unless `everyone`;
/// every kept commit rolls its raw author up to the unified `me` identity.
fn build_repo_graph(
    e: &RegistryEntry,
    commits: &[RawCommit],
    me: &MeConfig,
    everyone: bool,
    bounds: (Option<i64>, Option<i64>),
) -> (CodeGraph, Vec<CommitMeta>, usize, usize) {
    let (since, until) = bounds;
    let mut g = CodeGraph::new();
    let mut metas = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept = 0usize;
    let mut skipped = 0usize;

    // Repo node, tagged with its visibility tier (#332).
    let repo_id = NodeId::derive(&["repo", &e.name]);
    push_node(
        &mut g,
        &mut seen,
        Node {
            signature: Some(e.visibility.as_str().to_string()),
            ..atlas_node(
                repo_id.clone(),
                NodeKind::Repo,
                e.name.clone(),
                e.name.clone(),
            )
        },
    );
    let me_id = NodeId::derive(&["identity", "me"]);

    for c in commits {
        let mine = me.matches(&c.author_email);
        if !everyone && !mine {
            skipped += 1;
            continue;
        }
        kept += 1;
        let email = c.author_email.trim().to_ascii_lowercase();
        let subject = scrub_secrets(&c.subject).into_owned();
        // Heuristic topics from the scrubbed subject + touched paths (#334).
        let topics = glyphtrail_core::derive_topics(&subject, &c.files);

        let commit_id = NodeId::derive(&["commit", &e.name, &c.hash]);
        push_node(
            &mut g,
            &mut seen,
            atlas_node(
                commit_id.clone(),
                NodeKind::Commit,
                subject.clone(),
                c.hash.clone(),
            ),
        );
        metas.push(CommitMeta {
            node_id: commit_id.clone(),
            hash: c.hash.clone(),
            author_email: email.clone(),
            committed_at: c.committed_at,
            subject,
            in_bounds: within(c.committed_at, since, until),
        });
        g.add_edge(
            commit_id.clone(),
            repo_id.clone(),
            EdgeKind::PartOf,
            Confidence::Extracted,
        );

        // Author -> Authored -> Commit; Author -> AliasOf -> Identity.
        let author_id = NodeId::derive(&["author", &email]);
        push_node(
            &mut g,
            &mut seen,
            atlas_node(
                author_id.clone(),
                NodeKind::Author,
                format!("{} <{}>", c.author_name, email),
                email.clone(),
            ),
        );
        g.add_edge(
            author_id.clone(),
            commit_id.clone(),
            EdgeKind::Authored,
            Confidence::Extracted,
        );
        let identity_id = if mine {
            push_node(
                &mut g,
                &mut seen,
                atlas_node(
                    me_id.clone(),
                    NodeKind::Identity,
                    me.display().unwrap_or_else(|| "me".into()),
                    "me".into(),
                ),
            );
            me_id.clone()
        } else {
            let id = NodeId::derive(&["identity", &email]);
            push_node(
                &mut g,
                &mut seen,
                atlas_node(
                    id.clone(),
                    NodeKind::Identity,
                    format!("{} <{}>", c.author_name, email),
                    email.clone(),
                ),
            );
            id
        };
        g.add_edge(
            author_id.clone(),
            identity_id,
            EdgeKind::AliasOf,
            Confidence::Extracted,
        );

        // Touched files: repo-qualified File nodes, each part of the repo.
        for path in &c.files {
            let file_id = NodeId::derive(&["file", &e.name, path]);
            push_node(
                &mut g,
                &mut seen,
                Node {
                    file: path.clone(),
                    ..atlas_node(
                        file_id.clone(),
                        NodeKind::File,
                        path.clone(),
                        format!("{}/{}", e.name, path),
                    )
                },
            );
            g.add_edge(
                commit_id.clone(),
                file_id.clone(),
                EdgeKind::Touched,
                Confidence::Extracted,
            );
            g.add_edge(
                file_id,
                repo_id.clone(),
                EdgeKind::PartOf,
                Confidence::Extracted,
            );
        }

        // Topic tags: a shared Topic node per derived keyword, the commit tagged
        // with each (#334). Topics span repos, so they merge across the atlas.
        for topic in &topics {
            let topic_id = NodeId::derive(&["topic", topic]);
            push_node(
                &mut g,
                &mut seen,
                atlas_node(
                    topic_id.clone(),
                    NodeKind::Topic,
                    topic.clone(),
                    topic.clone(),
                ),
            );
            g.add_edge(
                commit_id.clone(),
                topic_id,
                EdgeKind::Tagged,
                Confidence::Inferred,
            );
        }
    }
    (g, metas, kept, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn parse_log_reads_fields_and_touched_files() {
        // Two commits; the first touches two files, the second none. Fields are
        // unit-separated, commits record-separated, exactly as `git log` emits.
        let text = "\u{1e}abc\u{1f}1700000000\u{1f}Ada\u{1f}ada@x.dev\u{1f}Add parser\n\
                    src/lib.rs\nsrc/main.rs\n\
                    \u{1e}def\u{1f}1699999999\u{1f}Grace\u{1f}grace@y.dev\u{1f}Init";
        let commits = parse_log(text);
        check!(commits.len() == 2);
        check!(commits[0].hash == "abc");
        check!(commits[0].committed_at == 1_700_000_000);
        check!(commits[0].author_email == "ada@x.dev");
        check!(commits[0].subject == "Add parser");
        check!(commits[0].files == vec!["src/lib.rs", "src/main.rs"]);
        check!(commits[1].hash == "def");
        check!(commits[1].files.is_empty());
        // A malformed timestamp drops the record rather than panicking.
        check!(parse_log("\u{1e}h\u{1f}notanint\u{1f}A\u{1f}a@x\u{1f}s").is_empty());
    }

    #[test]
    fn build_repo_graph_keeps_mine_and_rolls_authors_to_one_identity() {
        let me = MeConfig {
            emails: vec!["ada@x.dev".into()],
            domains: vec!["mine.dev".into()],
        };
        let entry = RegistryEntry {
            name: "proj".into(),
            root: "/tmp/proj".into(),
            alt_roots: vec![],
            missing_since: None,
            ids: vec![],
            contributors: vec![],
            identity: None,
            visibility: glyphtrail_core::Visibility::Private,
        };
        let commits = vec![
            raw("c1", 1_500_000_000, "Ada", "ada@x.dev", "first", &["a.rs"]),
            // Same person via an owned-domain alias -> same identity.
            raw(
                "c2",
                1_500_000_100,
                "Ada Alt",
                "ada@mine.dev",
                "second",
                &["b.rs"],
            ),
            // A stranger -> skipped in mine-only mode.
            raw(
                "c3",
                1_500_000_200,
                "Eve",
                "eve@evil.dev",
                "third",
                &["c.rs"],
            ),
        ];
        let (g, metas, kept, skipped) =
            build_repo_graph(&entry, &commits, &me, false, (None, None));
        check!(kept == 2 && skipped == 1);
        check!(metas.len() == 2);
        // Two distinct raw authors, both AliasOf the single "me" identity.
        let identities: HashSet<_> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Identity)
            .map(|n| n.id.0.clone())
            .collect();
        check!(identities.len() == 1);
        let authors = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Author)
            .count();
        check!(authors == 2);
        // Repo node carries the visibility tier.
        let repo = g.nodes.iter().find(|n| n.kind == NodeKind::Repo).unwrap();
        check!(repo.signature.as_deref() == Some("private"));
        // Touched edges only for the kept commits' files.
        let touched = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Touched)
            .count();
        check!(touched == 2);
    }

    #[test]
    fn truncate_caps_and_ellipsizes() {
        check!(truncate("short", 20) == "short");
        check!(truncate("0123456789", 5) == "0123…");
    }

    #[test]
    fn atlas_story_prompt_lists_repos_and_oldest_first_log() {
        let row = |repo: &str, at: i64, subject: &str| glyphtrail_core::AtlasTimelineRow {
            commit: CommitMeta {
                node_id: NodeId("x".into()),
                hash: "h".into(),
                author_email: "ada@x.dev".into(),
                committed_at: at,
                subject: subject.into(),
                in_bounds: true,
            },
            repo: repo.into(),
            touched: 1,
        };
        // As `filter_timeline` yields them: newest first.
        let tl = glyphtrail_core::Timeline {
            rows: vec![
                row("beta", 1_600_000_100, "second thing"),
                row("alpha", 1_600_000_000, "first thing"),
            ],
            matched: 2,
            excluded_restricted: 0,
            excluded_author: 0,
        };
        let p = atlas_story_prompt(&tl, "2020-09-01..");
        check!(p.contains("Window: 2020-09-01.."));
        check!(p.contains("Repositories (2): alpha, beta"));
        check!(p.contains("Commits: 2"));
        // Log is oldest-first: "first thing" precedes "second thing".
        let first = p.find("first thing").unwrap();
        let second = p.find("second thing").unwrap();
        check!(first < second);
        check!(p.contains("how my work evolved"));
    }

    fn raw(
        hash: &str,
        committed_at: i64,
        name: &str,
        email: &str,
        subject: &str,
        files: &[&str],
    ) -> RawCommit {
        RawCommit {
            hash: hash.into(),
            committed_at,
            author_name: name.into(),
            author_email: email.into(),
            subject: subject.into(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }
}
