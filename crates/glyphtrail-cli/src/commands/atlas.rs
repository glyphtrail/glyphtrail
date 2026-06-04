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
use glyphtrail_core::config::RepoPaths;
use glyphtrail_core::{
    AtlasConfig, AtlasHeads, CodeGraph, CommitMeta, Confidence, EdgeKind, Embedding, GraphEmbedder,
    GraphProfile, MeConfig, Node, NodeId, NodeKind, Registry, RegistryEntry, StructuralEmbedder,
    TimelineQuery, Window, author_scope_label, cosine, default_atlas_path, default_registry_path,
    filter_timeline, format_date, scrub_secrets, timeline_value,
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
    /// Compute repo embeddings from the synced history (local lexical model, #338).
    Embed(EmbedArgs),
    /// Compute structural embeddings from each repo's code graph (local, #338).
    GraphEmbed(GraphEmbedArgs),
    /// Embed every synced commit (by subject) into an HNSW index (#338).
    EmbedCommits(EmbedCommitsArgs),
    /// Find repos similar to a repo name or free-text query (visibility-gated).
    Similar(SimilarArgs),
    /// Find commits similar to a free-text query, across repos (visibility-gated).
    SimilarCommits(SimilarCommitsArgs),
    /// Serve the atlas over MCP (stdio): timeline, status, and the repo+file bridge.
    Mcp,
}

#[derive(Args)]
pub struct EmbedArgs {
    /// Embedding vector width for the local provider (the hashing-trick bucket
    /// count). Ignored by API providers, which set their own dimension.
    #[arg(long, default_value_t = glyphtrail_core::DEFAULT_DIM)]
    pub dim: usize,
    /// Embedding provider. `local` is the default and never leaves the machine;
    /// `openai` POSTs commit text to an OpenAI-compatible endpoint.
    #[arg(long, value_enum, default_value_t)]
    pub provider: crate::commands::embed_provider::EmbedProvider,
    /// Embedding model id (provider default when unset).
    #[arg(long)]
    pub model: Option<String>,
    /// Override the embeddings endpoint (e.g. a local Ollama server) for `openai`.
    #[arg(long)]
    pub base_url: Option<String>,
}

#[derive(Args)]
pub struct GraphEmbedArgs {
    /// Embedding vector width (the hashing-trick bucket count).
    #[arg(long, default_value_t = glyphtrail_core::DEFAULT_DIM)]
    pub dim: usize,
}

#[derive(Args)]
pub struct EmbedCommitsArgs {
    /// Embedding vector width for the local provider.
    #[arg(long, default_value_t = glyphtrail_core::DEFAULT_DIM)]
    pub dim: usize,
    /// Embedding provider (`local` never leaves the machine; `openai` POSTs commit
    /// subjects to an OpenAI-compatible endpoint).
    #[arg(long, value_enum, default_value_t)]
    pub provider: crate::commands::embed_provider::EmbedProvider,
    /// Embedding model id (provider default when unset).
    #[arg(long)]
    pub model: Option<String>,
    /// Override the embeddings endpoint (e.g. a local Ollama server) for `openai`.
    #[arg(long)]
    pub base_url: Option<String>,
}

#[derive(Args)]
pub struct SimilarCommitsArgs {
    /// Free-text query; the nearest commits across repos are returned.
    pub query: String,
    /// Restrict to one registered repo.
    #[arg(long)]
    pub repo: Option<String>,
    /// Which embedding model to search (default: the most-recently-embedded one).
    #[arg(long)]
    pub model: Option<String>,
    /// How many commits to show (most similar first).
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Include restricted repos — private, proprietary, or unregistered (excluded
    /// by default).
    #[arg(long)]
    pub include_restricted: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
    /// Emit YAML.
    #[arg(long)]
    pub yaml: bool,
}

#[derive(Args)]
pub struct SimilarArgs {
    /// A registered repo name (repo↔repo similarity) or free text (query↔repo).
    pub query: String,
    /// Compare repos by code-graph structure (from `atlas graph-embed`) instead of
    /// commit text. Free-text queries aren't structural, so a repo name is required.
    #[arg(long)]
    pub graph: bool,
    /// Which embedding model to search (default: the most-recently-embedded one for
    /// this space). Models never mix; see `atlas status` for what's stored.
    #[arg(long)]
    pub model: Option<String>,
    /// How many matches to show (most similar first).
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Include restricted repos — private, proprietary, or unregistered (excluded
    /// by default).
    #[arg(long)]
    pub include_restricted: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
    /// Emit YAML.
    #[arg(long)]
    pub yaml: bool,
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
        AtlasCmd::Embed(args) => embed(&dir, args)?,
        AtlasCmd::GraphEmbed(args) => graph_embed(&dir, args)?,
        AtlasCmd::EmbedCommits(args) => embed_commits(&dir, args)?,
        AtlasCmd::Similar(args) => similar(&dir, args)?,
        AtlasCmd::SimilarCommits(args) => similar_commits(&dir, args)?,
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

    let embeds = store.embedding_index()?;

    println!("atlas:   enabled");
    println!("path:    {}", lb.display());
    println!("nodes:   {}", stats.nodes);
    println!("edges:   {}", stats.edges);
    println!("commits: {commits}");
    if embeds.is_empty() {
        println!("embeds:  none (run `glyphtrail atlas embed` / `graph-embed` / `embed-commits`)");
    } else {
        println!("embeds:");
        for (space, model, count, dim) in &embeds {
            println!("  {space:<7} {model:<32} {count} vec, dim {dim}");
        }
    }
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

/// The atlas `Repo` node id for a registry name (mirrors `sync`'s id scheme).
fn repo_node_id(name: &str) -> NodeId {
    NodeId::derive(&["repo", name])
}

/// The key a repo's *structural* embedding is stored under — a separate id space
/// from the text embedding, so both coexist in the one side-table (#338).
fn repo_graph_node_id(name: &str) -> NodeId {
    NodeId::derive(&["repo_graph", name])
}

/// `glyphtrail atlas graph-embed` — embed each registered repo's code-graph
/// structure (node/edge-kind + language histograms) so `atlas similar --graph`
/// ranks repos by architecture (#338). Fully local: reads each repo's own
/// `.glyphtrail` index, no network. A repo with no index is skipped.
fn graph_embed(dir: &Path, args: GraphEmbedArgs) -> Result<()> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let registry = match default_registry_path() {
        Some(p) => Registry::load(&p)?,
        None => Registry::default(),
    };
    let embedder = StructuralEmbedder::new(args.dim);
    let mut embeddings: Vec<Embedding> = Vec::new();
    let mut no_index = 0usize; // missing dir or unreadable index
    let mut empty = 0usize; // index present but no structure
    for entry in &registry.repos {
        let repo_lb = RepoPaths::new(entry.active_root())
            .index_dir
            .join("ladybug");
        if !repo_lb.exists() {
            no_index += 1;
            continue;
        }
        // Read-only open: never trigger the schema migration (a drop+recreate),
        // which would wipe an out-of-date index just to read its kind counts.
        let Ok(repo_store) = LadybugStore::open_read_only(&repo_lb) else {
            no_index += 1;
            continue;
        };
        let profile = GraphProfile {
            node_kinds: repo_store.node_kind_counts()?,
            edge_kinds: repo_store.edge_kind_counts()?,
            languages: repo_store.stats()?.languages,
        };
        let vector = embedder.embed(&profile);
        if vector.iter().all(|x| *x == 0.0) {
            empty += 1; // an empty / unanalyzed graph carries no structure
            continue;
        }
        embeddings.push(Embedding {
            node_id: repo_graph_node_id(&entry.name),
            vector,
        });
    }
    if embeddings.is_empty() {
        bail!("no indexed repos to embed; run `glyphtrail analyze` in your repos first");
    }
    let mut store = LadybugStore::open(&lb)?;
    store.clear_embeddings_for(SPACE_GRAPH, embedder.id())?;
    store.set_embeddings(SPACE_GRAPH, embedder.id(), &embeddings)?;
    set_active_model(
        &mut store,
        SPACE_GRAPH,
        embedder.id(),
        None,
        embeddings[0].vector.len(),
    )?;
    let ann = build_vector_index(&store, &vec_table(SPACE_GRAPH, embedder.id()), &embeddings);
    println!(
        "graph-embedded {} repo{} ({} no index, {} empty) [{}{}]",
        embeddings.len(),
        if embeddings.len() == 1 { "" } else { "s" },
        no_index,
        empty,
        embedder.id(),
        if ann { ", HNSW ANN" } else { "" },
    );
    Ok(())
}

/// `glyphtrail atlas embed` — compute an embedding per repo from the synced commit
/// history and store it in the side table (#338). The default `local` provider
/// never leaves the machine; an `openai` provider POSTs the per-repo commit-subject
/// summaries to the configured endpoint (announced first), one of the few atlas
/// functions allowed off-machine on explicit opt-in. Embeds every repo regardless
/// of visibility (gating is at `similar` output).
fn embed(dir: &Path, args: EmbedArgs) -> Result<()> {
    use crate::commands::embed_provider::{EmbedConfig, embed_docs};
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let store = LadybugStore::open(&lb)?;
    // Every in-bounds commit, joined to its repo; build one document per repo from
    // the repo name plus its commit subjects (already secret-scrubbed at sync).
    let rows = store.atlas_timeline(None, None, None)?;
    let mut docs: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for row in rows {
        let doc = docs
            .entry(row.repo.clone())
            .or_insert_with(|| row.repo.clone());
        doc.push(' ');
        doc.push_str(&row.commit.subject);
    }
    if docs.is_empty() {
        bail!("no commits to embed; run `glyphtrail atlas sync` first");
    }
    let cfg = EmbedConfig {
        provider: args.provider,
        model: args.model.clone(),
        base_url: args.base_url.clone(),
        dim: args.dim,
    };
    if cfg.is_offmachine() {
        eprintln!(
            "atlas embed: sending {} repo summaries off-machine to {} ({})",
            docs.len(),
            crate::commands::embed_provider::host_of(&cfg.endpoint()),
            cfg.describe(),
        );
    }
    let names: Vec<String> = docs.keys().cloned().collect();
    let texts: Vec<String> = docs.values().cloned().collect();
    let vectors = embed_docs(&cfg, &texts)?;
    let embeddings: Vec<Embedding> = names
        .into_iter()
        .zip(vectors)
        .map(|(name, vector)| Embedding {
            node_id: repo_node_id(&name),
            vector,
        })
        .collect();
    let mut store = store;
    let model = cfg.model_id();
    // Replace just this (text, model) namespace — other models and the graph/commit
    // spaces coexist untouched, so a model upgrade never mixes with the old set.
    store.clear_embeddings_for(SPACE_TEXT, &model)?;
    store.set_embeddings(SPACE_TEXT, &model, &embeddings)?;
    set_active_model(
        &mut store,
        SPACE_TEXT,
        &model,
        cfg.base_url.as_deref(),
        embeddings[0].vector.len(),
    )?;
    let ann = build_vector_index(&store, &vec_table(SPACE_TEXT, &model), &embeddings);
    println!(
        "embedded {} repo{} ({}) [model {}{}]",
        embeddings.len(),
        if embeddings.len() == 1 { "" } else { "s" },
        cfg.describe(),
        model,
        if ann { ", HNSW ANN" } else { "" },
    );
    Ok(())
}

/// Embedding spaces (what + how was embedded) — the first half of an embedding
/// namespace; the model id is the second (#338).
const SPACE_TEXT: &str = "text";
const SPACE_GRAPH: &str = "graph";
const SPACE_COMMIT: &str = "commit";

/// The per-`(space, model)` FLOAT[]/HNSW table name, sanitised to kuzu's
/// `[A-Za-z0-9_]` identifier charset so each model gets its own index.
fn vec_table(space: &str, model: &str) -> String {
    let slug: String = model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("Vec_{space}_{slug}")
}

/// Record the most-recently-embedded model for a space (the `similar` default),
/// plus the provider base URL + dim, so a free-text query re-embeds the same way.
fn set_active_model(
    store: &mut LadybugStore,
    space: &str,
    model: &str,
    base_url: Option<&str>,
    dim: usize,
) -> Result<()> {
    store.set_meta(&format!("active_model_{space}"), model)?;
    store.set_meta(
        &format!("active_base_url_{space}"),
        base_url.unwrap_or_default(),
    )?;
    store.set_meta(&format!("active_dim_{space}"), &dim.to_string())?;
    Ok(())
}

/// Resolve which model a query searches in `space`: the explicit `--model`, else the
/// active (last-embedded) one, else the sole stored model — erroring with the list
/// of choices when none/ambiguous.
fn resolve_model(store: &LadybugStore, space: &str, explicit: Option<&str>) -> Result<String> {
    let models: Vec<String> = store
        .embedding_index()?
        .into_iter()
        .filter(|(sp, ..)| sp == space)
        .map(|(_, m, ..)| m)
        .collect();
    if models.is_empty() {
        bail!("no {space} embeddings yet; run the matching embed command first");
    }
    if let Some(m) = explicit {
        if models.iter().any(|x| x == m) {
            return Ok(m.to_string());
        }
        bail!(
            "no {space} embeddings for model '{m}'; available: {}",
            models.join(", ")
        );
    }
    if let Some(active) = store.get_meta(&format!("active_model_{space}"))?
        && models.iter().any(|x| x == &active)
    {
        return Ok(active);
    }
    if models.len() == 1 {
        return Ok(models[0].clone());
    }
    bail!(
        "multiple {space} embedding models stored; choose one with --model: {}",
        models.join(", ")
    );
}

/// `glyphtrail atlas embed-commits` — embed every in-bounds commit (by subject)
/// into an HNSW index, so `atlas similar-commits` can find commits like a query
/// (#338). This is the per-commit, ANN-scale path, so it requires the lbug vector
/// extension. `local` never leaves the machine; `openai` POSTs commit subjects.
fn embed_commits(dir: &Path, args: EmbedCommitsArgs) -> Result<()> {
    use crate::commands::embed_provider::{EmbedConfig, embed_docs, host_of};
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let mut store = LadybugStore::open(&lb)?;
    let rows = store.atlas_timeline(None, None, None)?;
    if rows.is_empty() {
        bail!("no commits to embed; run `glyphtrail atlas sync` first");
    }
    if !store.install_vector_ext() {
        bail!(
            "commit embeddings need the lbug vector extension (HNSW); it couldn't be \
             installed/loaded — see #473"
        );
    }
    let cfg = EmbedConfig {
        provider: args.provider,
        model: args.model.clone(),
        base_url: args.base_url.clone(),
        dim: args.dim,
    };
    if cfg.is_offmachine() {
        eprintln!(
            "atlas embed-commits: sending {} commit subjects off-machine to {} ({})",
            rows.len(),
            host_of(&cfg.endpoint()),
            cfg.describe(),
        );
    }
    let docs: Vec<String> = rows.iter().map(|r| r.commit.subject.clone()).collect();
    let vectors = embed_docs(&cfg, &docs)?;
    // Skip commits whose subject carries no signal (a zero vector has no defined
    // cosine and would poison the index).
    let embeddings: Vec<Embedding> = rows
        .iter()
        .zip(vectors)
        .filter(|(_, v)| v.iter().any(|x| *x != 0.0))
        .map(|(r, vector)| Embedding {
            node_id: r.commit.node_id.clone(),
            vector,
        })
        .collect();
    if embeddings.is_empty() {
        bail!("no commit subjects produced a usable embedding");
    }
    let dim = embeddings[0].vector.len();
    let model = cfg.model_id();
    // Durable rows (offline + export) plus the per-model HNSW index, namespaced so
    // a model upgrade coexists with the prior commit embeddings.
    store.clear_embeddings_for(SPACE_COMMIT, &model)?;
    store.set_embeddings(SPACE_COMMIT, &model, &embeddings)?;
    store.rebuild_vector_index(&vec_table(SPACE_COMMIT, &model), &embeddings)?;
    set_active_model(
        &mut store,
        SPACE_COMMIT,
        &model,
        cfg.base_url.as_deref(),
        dim,
    )?;
    println!(
        "embedded {} commits ({}) [model {model}, HNSW ANN]",
        embeddings.len(),
        cfg.describe(),
    );
    Ok(())
}

/// `glyphtrail atlas similar-commits` — rank commits across repos by similarity to
/// a free-text query, gating restricted repos out of the result (#338).
fn similar_commits(dir: &Path, args: SimilarCommitsArgs) -> Result<()> {
    use crate::commands::embed_provider::{config_from_stored, embed_one, host_of};
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let store = LadybugStore::open(&lb)?;
    if !store.load_vector_ext() {
        bail!(
            "commit similarity needs the lbug vector extension; run `glyphtrail atlas embed-commits` first"
        );
    }
    let model = resolve_model(&store, SPACE_COMMIT, args.model.as_deref())?;
    let base_url = store
        .get_meta("active_base_url_commit")?
        .filter(|s| !s.is_empty());
    let dim = store
        .get_meta("active_dim_commit")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(glyphtrail_core::DEFAULT_DIM);
    let qcfg = config_from_stored(&model, base_url, dim);
    if qcfg.is_offmachine() {
        eprintln!(
            "atlas similar-commits: sending the query off-machine to {} ({})",
            host_of(&qcfg.endpoint()),
            qcfg.describe(),
        );
    }
    let qvec = embed_one(&qcfg, &args.query)?;
    if qvec.iter().all(|x| *x == 0.0) {
        bail!("the query has no searchable terms after tokenization; try different words");
    }

    // Over-fetch from the chosen model's HNSW index, then map *only those* commits
    // back to their repo/date/subject (not the whole timeline) and gate by repo
    // visibility before truncating to the requested limit.
    let hits = store.vector_knn(&vec_table(SPACE_COMMIT, &model), &qvec, args.limit * 4 + 32)?;
    let ids: Vec<String> = hits.iter().map(|(id, _)| id.0.clone()).collect();
    let rows = store.atlas_commit_rows(&ids)?;
    let by_id: std::collections::HashMap<String, &glyphtrail_core::AtlasTimelineRow> = rows
        .iter()
        .map(|r| (r.commit.node_id.0.clone(), r))
        .collect();
    let registry = match default_registry_path() {
        Some(p) => Registry::load(&p)?,
        None => Registry::default(),
    };

    let mut hidden = 0usize;
    let mut out: Vec<(f32, String, i64, String)> = Vec::new();
    for (id, sim) in &hits {
        let Some(row) = by_id.get(&id.0) else {
            continue;
        };
        if let Some(repo) = &args.repo
            && &row.repo != repo
        {
            continue;
        }
        let restricted = registry
            .get(&row.repo)
            .map(|e| e.visibility.is_restricted())
            .unwrap_or(true);
        if restricted && !args.include_restricted {
            hidden += 1;
            continue;
        }
        out.push((
            *sim,
            row.repo.clone(),
            row.commit.committed_at,
            row.commit.subject.clone(),
        ));
        if out.len() >= args.limit {
            break;
        }
    }

    let emit = Emit::from_flags(args.json, args.yaml);
    if emit == Emit::Text {
        println!("atlas similar-commits — query \"{}\"", args.query);
        if hidden > 0 {
            println!("  hidden:  {hidden} restricted (use --include-restricted)");
        }
        println!("  showing: {}", out.len());
        println!();
        for (i, (sim, repo, at, subject)) in out.iter().enumerate() {
            println!(
                "  {:>2}. {:6.3}  {}  {:<16}  {}",
                i + 1,
                sim,
                format_date(*at),
                truncate(repo, 16),
                subject,
            );
        }
        if out.is_empty() {
            println!("  (no matches)");
        }
    } else {
        let matches: Vec<serde_json::Value> = out
            .iter()
            .map(|(sim, repo, at, subject)| {
                serde_json::json!({
                    "score": sim,
                    "repo": repo,
                    "date": format_date(*at),
                    "subject": subject,
                })
            })
            .collect();
        print_value(
            &serde_json::json!({
                "query": args.query,
                "hidden_restricted": hidden,
                "matches": matches,
            }),
            emit,
        )?;
    }
    Ok(())
}

/// Build the HNSW vector index for a space if the lbug vector extension is
/// available (installing it on first use — a one-time, documented network fetch).
/// Returns whether the index was built; a failure is non-fatal (the brute-force
/// path in `similar` still works). #338/#473.
fn build_vector_index(store: &LadybugStore, table: &str, embeddings: &[Embedding]) -> bool {
    if !store.install_vector_ext() {
        return false;
    }
    match store.rebuild_vector_index(table, embeddings) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("note: HNSW vector index not built ({e}); similarity falls back to a scan");
            false
        }
    }
}

/// `glyphtrail atlas similar` — rank repos by lexical similarity to a repo name or
/// free-text query, gating restricted repos out of the output (#338).
fn similar(dir: &Path, args: SimilarArgs) -> Result<()> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        bail!("atlas is disabled; run `glyphtrail atlas init` first");
    }
    let store = LadybugStore::open(&lb)?;
    let registry = match default_registry_path() {
        Some(p) => Registry::load(&p)?,
        None => Registry::default(),
    };
    // Pick the (space, model) namespace: graph vs text, and the model (--model, else
    // the active/default one). Read just that namespace — models never mix.
    let space = if args.graph { SPACE_GRAPH } else { SPACE_TEXT };
    let model = resolve_model(&store, space, args.model.as_deref())?;
    let embeddings = store.embeddings_for(space, &model)?;
    if embeddings.is_empty() {
        bail!("no {space} embeddings for model '{model}'; run the matching embed command");
    }
    let dim = embeddings[0].vector.len();

    // Registry id→entry map in this space's id scheme, for naming + visibility.
    let id_of = |name: &str| -> NodeId {
        if args.graph {
            repo_graph_node_id(name)
        } else {
            repo_node_id(name)
        }
    };
    let by_id: std::collections::HashMap<String, &RegistryEntry> = registry
        .repos
        .iter()
        .map(|e| (id_of(&e.name).0, e))
        .collect();

    // Resolve the query vector: a known repo uses its stored embedding (excluded
    // from its own results). For text, anything else is embedded as a free-text
    // query under this model's provider; structural similarity needs a repo.
    let self_id = id_of(&args.query).0;
    let (qvec, self_id, mode) = if let Some(e) = embeddings.iter().find(|e| e.node_id.0 == self_id)
    {
        (
            e.vector.clone(),
            Some(self_id),
            format!("repo '{}'", args.query),
        )
    } else if args.graph {
        bail!(
            "graph similarity compares repositories; pass a registered repo name \
             (run `glyphtrail atlas graph-embed` if it isn't embedded yet)"
        );
    } else {
        let base_url = store
            .get_meta(&format!("active_base_url_{space}"))?
            .filter(|s| !s.is_empty());
        let qcfg = crate::commands::embed_provider::config_from_stored(&model, base_url, dim);
        if qcfg.is_offmachine() {
            eprintln!(
                "atlas similar: sending the query off-machine to {} ({})",
                crate::commands::embed_provider::host_of(&qcfg.endpoint()),
                qcfg.describe(),
            );
        }
        let v = crate::commands::embed_provider::embed_one(&qcfg, &args.query)?;
        (v, None, format!("query \"{}\"", args.query))
    };

    // Candidate (node id, similarity) pairs: prefer this (space,model)'s HNSW index;
    // fall back to an exact in-Rust cosine scan when the extension isn't available.
    let table = vec_table(space, &model);
    let candidates: Vec<(String, f32)> = store
        .load_vector_ext()
        .then(|| store.vector_knn(&table, &qvec, embeddings.len()).ok())
        .flatten()
        .filter(|h| !h.is_empty())
        .map(|h| h.into_iter().map(|(id, sim)| (id.0, sim)).collect())
        .unwrap_or_else(|| {
            embeddings
                .iter()
                .map(|e| (e.node_id.0.clone(), cosine(&qvec, &e.vector)))
                .collect()
        });

    let mut excluded = 0usize;
    let mut scored: Vec<(f32, String, &'static str)> = Vec::new();
    for (id, sim) in &candidates {
        if Some(id) == self_id.as_ref() {
            continue;
        }
        let entry = by_id.get(id);
        let restricted = entry.map(|x| x.visibility.is_restricted()).unwrap_or(true);
        if restricted && !args.include_restricted {
            excluded += 1;
            continue;
        }
        // The repo name can't be recovered from the one-way node id, so tag an
        // unregistered row with a short id prefix to keep multiple ones distinct.
        let name = entry.map(|x| x.name.clone()).unwrap_or_else(|| {
            let short: String = id.chars().take(8).collect();
            format!("(unregistered {short})")
        });
        let vis = match entry.map(|x| x.visibility) {
            Some(v) => v.as_str(),
            None => "unregistered",
        };
        scored.push((*sim, name, vis));
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored.truncate(args.limit);

    let model_label = model;
    let emit = Emit::from_flags(args.json, args.yaml);
    if emit == Emit::Text {
        println!("atlas similar — {mode}");
        println!("  model:   {model_label}");
        if excluded > 0 {
            println!(
                "  hidden:  {excluded} restricted (private/proprietary/unregistered; use --include-restricted)"
            );
        }
        println!("  showing: {}", scored.len());
        println!();
        for (i, (score, name, vis)) in scored.iter().enumerate() {
            println!(
                "  {:>2}. {:6.3}  {:<24}  [{}]",
                i + 1,
                score,
                truncate(name, 24),
                vis
            );
        }
        if scored.is_empty() {
            println!("  (no matches)");
        }
    } else {
        let matches: Vec<serde_json::Value> = scored
            .iter()
            .map(|(score, name, vis)| {
                serde_json::json!({ "repo": name, "score": score, "visibility": vis })
            })
            .collect();
        print_value(
            &serde_json::json!({
                "query": args.query,
                "mode": mode,
                "model": model_label,
                "hidden_restricted": excluded,
                "matches": matches,
            }),
            emit,
        )?;
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
