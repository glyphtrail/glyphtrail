use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use glyphtrail_core::{
    RecordOutcome, Registry, RegistryEntry, RepoHealth, Resolution, default_registry_path,
    filelock, lock_path, repo_ids,
};
use glyphtrail_forge_id::{ForgeConfig, forge_numeric_ids};
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Subcommand)]
pub enum RepoCmd {
    /// Register a repository in the global registry.
    Add {
        /// Repository root (defaults to the current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Name for the repo (defaults to the directory name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Recursively find repositories under a directory and register them.
    ///
    /// Walks the tree for version-control roots (`.git`, `.svn`, `.bzr`, `.hg`).
    /// By default only repos that are already indexed are registered (so a repo
    /// whose earlier registration failed is recovered without pulling in every
    /// stray checkout); use `--analyze` to index each as it's found, or `--all`
    /// to register everything regardless. Handy for pointing at a whole
    /// workspace at once.
    Scan {
        /// Directory to scan (defaults to the current directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Analyze each repo as it's found (then register it). `--update` keeps
        /// it incremental.
        #[arg(long)]
        analyze: bool,
        /// With --analyze, only reparse files changed since the last index.
        #[arg(long)]
        update: bool,
        /// Register every repo found, including ones with no index yet. By
        /// default only already-indexed repos are registered.
        #[arg(long)]
        all: bool,
        /// Descend into repositories to also find nested repos (submodules,
        /// vendored checkouts). By default a repo root is a boundary.
        #[arg(long)]
        recursive: bool,
        /// Descend into dot-directories (`.git`, `.cache`, `.claude`, …). Off by
        /// default so worktrees and tool caches aren't scanned.
        #[arg(long)]
        hidden: bool,
    },
    /// List registered repositories and their health.
    List {
        /// Only show repos with a contributor whose name or email matches this
        /// substring (case-insensitive).
        #[arg(long)]
        author: Option<String>,
        /// Only show repos you've contributed to (matches your `git config
        /// user.email`). Shorthand for `--author <your-email>`.
        #[arg(long)]
        mine: bool,
    },
    /// Remove a repository from the registry by name.
    Remove { name: String },
    /// Drop registry entries whose root has been missing for too long.
    Prune {
        /// Minimum age (in days) a missing entry must reach before removal.
        #[arg(long, default_value_t = 30)]
        older_than_days: u64,
    },
    /// Re-derive forge identities for registered repos from their current git
    /// remotes, updating the registry in place.
    ///
    /// Useful after an id-format fix or when remotes change: it recomputes ids
    /// without re-analyzing. Entries whose root is missing are left untouched.
    Refresh {
        /// Only refresh this repo (defaults to all registered repos).
        name: Option<String>,
    },
    /// Set a repo's atlas visibility tier (#332): public, private, or proprietary.
    SetVisibility {
        /// Registered repo name.
        name: String,
        /// `public` | `private` | `proprietary`.
        visibility: String,
    },
    /// Force-release a stuck registry lock (escape hatch for a lock left by a
    /// dead writer on a network/FUSE filesystem). Safe: only removes the lock
    /// file; the registry self-heals stale locks automatically, so this is
    /// rarely needed.
    Unlock,
    /// Edit manual cross-repo link hints (glyphtrail.links.toml).
    Link {
        #[command(subcommand)]
        cmd: super::link::LinkCmd,
    },
}

pub(crate) fn registry_path() -> Result<PathBuf> {
    default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

/// Re-analyze every registered repo (`analyze --all`). Per-repo failures are
/// reported and don't abort the run.
pub fn analyze_all(update: bool) -> Result<()> {
    each_repo("analyze", |root| {
        let outcome = super::analyze::run(root, update)?;
        if outcome.ignored {
            println!("  skipped (excluded by ~/.glyphtrailignore)");
        } else if outcome.up_to_date {
            println!("  up to date ({} files)", outcome.files);
        } else {
            println!(
                "  {} files, {} nodes, {} edges",
                outcome.files, outcome.nodes, outcome.edges
            );
        }
        Ok(())
    })
}

/// Show index stats for every registered repo (`status --all`).
pub fn status_all() -> Result<()> {
    each_repo("status", |root| {
        super::status::run(root, super::query::Emit::Text)
    })
}

/// Run `op` over every registered repo, printing a per-repo header and a final
/// summary. Entries whose root is missing are skipped (not failed) so a single
/// moved/deleted repo doesn't drown the run in errors; their `missing_since`
/// stamp is refreshed and persisted for later `repo prune`.
fn each_repo(verb: &str, op: impl Fn(&std::path::Path) -> Result<()>) -> Result<()> {
    let path = registry_path()?;
    // Refresh + persist health under the lock, then release it before the long
    // per-repo work below (analysis must never run while holding the lock).
    let registry = Registry::mutate(&path, |reg| {
        reg.refresh_health();
        reg.clone()
    })?;
    if registry.repos.is_empty() {
        println!("(no repositories registered; use `glyphtrail repo add`)");
        return Ok(());
    }
    let (mut ok, mut failed, mut skipped) = (0u32, 0u32, 0u32);
    for e in &registry.repos {
        println!("== {} ({}) ==", e.name, e.active_root().display());
        if e.health() == RepoHealth::Missing {
            skipped += 1;
            println!("  skipped: root is missing");
            continue;
        }
        match op(e.active_root()) {
            Ok(()) => ok += 1,
            Err(err) => {
                failed += 1;
                eprintln!("  {}: {err:#}", e.name);
            }
        }
    }
    println!("{verb}: {ok} ok, {failed} failed, {skipped} skipped");
    Ok(())
}

pub fn run(cmd: RepoCmd) -> Result<()> {
    let path = registry_path()?;

    // Every arm that mutates the registry runs its load → modify → save under
    // an exclusive advisory lock (#129), so concurrent `repo` processes can't
    // clobber each other's update.
    match cmd {
        RepoCmd::Add { path: repo, name } => {
            let root = repo
                .canonicalize()
                .with_context(|| format!("cannot resolve path {}", repo.display()))?;
            register(&path, &root, name)?;
        }
        RepoCmd::Scan {
            dir,
            analyze,
            update,
            all,
            recursive,
            hidden,
        } => scan(
            &path,
            &dir,
            ScanOpts {
                analyze,
                update,
                all,
                recursive,
                hidden,
            },
        )?,
        RepoCmd::List { author, mine } => {
            let registry = Registry::mutate(&path, |reg| {
                reg.refresh_health();
                reg.clone()
            })?;
            // `--mine` is `--author <your git email>`; an explicit `--author`
            // wins. With neither, no contributor filter (#265).
            let filter = match (author, mine) {
                (Some(a), _) => Some(a),
                (None, true) => match git_user_email() {
                    Some(email) => Some(email),
                    None => bail!("`--mine` needs a git email; set `git config user.email`"),
                },
                (None, false) => None,
            };
            let shown: Vec<&RegistryEntry> = registry
                .repos
                .iter()
                .filter(|e| filter.as_deref().is_none_or(|f| e.has_contributor(f)))
                .collect();
            if shown.is_empty() {
                match &filter {
                    Some(f) => println!("(no registered repos with contributor matching '{f}')"),
                    None => println!("(no repositories registered)"),
                }
            }
            for e in shown {
                let note = match e.health() {
                    RepoHealth::Indexed => String::new(),
                    RepoHealth::Unindexed => "  (not indexed)".into(),
                    RepoHealth::Missing => {
                        format!("  (missing{})", missing_for(e.missing_since))
                    }
                };
                println!(
                    "{:<20} {}{}  [{}]",
                    e.name,
                    e.root.display(),
                    note,
                    e.visibility.as_str()
                );
                for alt in &e.alt_roots {
                    println!("{:<20} ↳ also at {}", "", alt.display());
                }
                for id in &e.ids {
                    println!("{:<20} ↳ {}", "", id.source);
                }
            }
        }
        RepoCmd::Remove { name } => {
            if Registry::mutate(&path, |reg| reg.remove(&name))? {
                println!("removed '{name}'");
            } else {
                bail!("no repository named '{name}' in the registry");
            }
        }
        RepoCmd::Prune { older_than_days } => {
            let removed = Registry::mutate(&path, |reg| {
                reg.refresh_health();
                reg.prune_missing(older_than_days as i64 * 86_400)
            })?;
            if removed.is_empty() {
                println!("nothing to prune (no entry missing for >= {older_than_days}d)");
            } else {
                for name in &removed {
                    println!("pruned '{name}'");
                }
            }
        }
        RepoCmd::Refresh { name } => refresh(&path, name.as_deref())?,
        RepoCmd::Unlock => match filelock::force_unlock(&lock_path(&path))? {
            Some(desc) => println!("released registry lock ({desc})"),
            None => println!("no registry lock held"),
        },
        RepoCmd::Link { cmd } => super::link::run(cmd)?,
        RepoCmd::SetVisibility { name, visibility } => {
            let vis = glyphtrail_core::Visibility::parse(&visibility).ok_or_else(|| {
                anyhow!("invalid visibility '{visibility}' (public|private|proprietary)")
            })?;
            let mut found = false;
            Registry::mutate(&path, |r| {
                if let Some(e) = r.repos.iter_mut().find(|e| e.name == name) {
                    e.visibility = vis;
                    found = true;
                }
            })?;
            if !found {
                bail!("no registered repo named '{name}'");
            }
            println!("{name}: visibility = {}", vis.as_str());
        }
    }
    Ok(())
}

/// Re-derive forge ids for registered repos from their current git remotes,
/// updating the registry in place. The slow per-repo git work runs outside the
/// lock; the registry is then re-loaded under the lock to apply the new ids, so
/// a concurrent writer isn't blocked while remotes are read (which can be slow
/// on a network drive). Entries whose root is missing are left untouched.
fn refresh(registry_path: &Path, only: Option<&str>) -> Result<()> {
    let registry = Registry::mutate(registry_path, |reg| {
        reg.refresh_health();
        reg.clone()
    })?;
    let targets: Vec<&RegistryEntry> = registry
        .repos
        .iter()
        .filter(|e| only.is_none_or(|n| e.name == n))
        .collect();
    if targets.is_empty() {
        match only {
            Some(n) => bail!("no repository named '{n}' in the registry"),
            None => {
                println!("(no repositories registered)");
                return Ok(());
            }
        }
    }

    let bar = ProgressBar::new(targets.len() as u64);
    bar.set_style(
        ProgressStyle::with_template("{spinner:.cyan} [{pos}/{len}] {wide_msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    bar.enable_steady_tick(std::time::Duration::from_millis(120));

    let (mut changed, mut unchanged, mut missing) = (0u32, 0u32, 0u32);
    let mut updates: Vec<(String, Vec<glyphtrail_core::RepoId>)> = Vec::new();
    for e in &targets {
        bar.set_message(e.name.clone());
        if e.roots().all(|r| !r.exists()) {
            missing += 1;
            bar.suspend(|| println!("{}: skipped (root is missing)", e.name));
            bar.inc(1);
            continue;
        }
        let fresh = entry_for(registry_path, e.active_root(), Some(e.name.clone()));
        if fresh.ids == e.ids {
            unchanged += 1;
        } else {
            changed += 1;
            let (name, old) = (e.name.clone(), e.ids.clone());
            let new = fresh.ids.clone();
            bar.suspend(|| {
                println!("{name}: ids updated");
                for id in &old {
                    println!("  - {}", id.source);
                }
                for id in &new {
                    println!("  + {}", id.source);
                }
            });
            updates.push((name, new));
        }
        bar.inc(1);
    }
    bar.finish_and_clear();

    if !updates.is_empty() {
        Registry::mutate(registry_path, |reg| {
            for (name, ids) in &updates {
                reg.set_ids(name, ids.clone());
            }
        })?;
    }
    println!("refresh: {changed} updated, {unchanged} unchanged, {missing} missing");
    Ok(())
}

/// Register (or re-register) `root` in the registry at `registry_path`, printing
/// the outcome and any forge ids. Shared by `repo add` and the remote-clone flow
/// (#291). Loss-proof: applies under the lock, or defers to a spillover file when
/// the lock is busy (a later run merges it) rather than failing — important when
/// registering many repos on a slow/contended network filesystem.
pub(crate) fn register(registry_path: &Path, root: &Path, name: Option<String>) -> Result<()> {
    let entry = entry_for(registry_path, root, name);
    let ids = entry.ids.clone();
    let name = entry.name.clone();
    match Registry::record(registry_path, vec![entry])? {
        RecordOutcome::Applied(res) => println!("{}", describe_record(&res[0], &name, root)),
        RecordOutcome::Spilled => println!(
            "registry busy; queued '{}' -> {} (merged on the next run)",
            name,
            root.display()
        ),
    }
    for id in &ids {
        println!("  id {} ({})", id.id, id.source);
    }
    Ok(())
}

/// Build a [`RegistryEntry`] for `root`: its name (the given override, else the
/// directory name) plus its stable forge identities (#233) derived from its git
/// remotes — slug ids always, forge-API numeric ids when a token is configured
/// (via `~/.glyphtrail/forge.toml`, the sibling of `registry_path`).
fn entry_for(registry_path: &Path, root: &Path, name: Option<String>) -> RegistryEntry {
    let name = name.unwrap_or_else(|| {
        root.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into())
    });
    let remotes = git_remote_urls(root);
    let mut ids = repo_ids(&remotes);
    let forge_config = ForgeConfig::load_or_default(&registry_path.with_file_name("forge.toml"));
    for numeric in forge_numeric_ids(&remotes, &forge_config) {
        if !ids.iter().any(|i| i.id == numeric.id) {
            ids.push(numeric);
        }
    }
    // Infer the atlas visibility tier from the forge-id sources (#332): a public
    // forge → Public, else Private; never Proprietary (explicit only).
    let visibility = glyphtrail_core::Visibility::infer(ids.iter().map(|i| i.source.as_str()));
    RegistryEntry {
        name,
        root: root.to_path_buf(),
        alt_roots: Vec::new(),
        missing_since: None,
        ids,
        contributors: git_contributors(root),
        identity: None,
        visibility,
    }
}

/// Cap on stored contributors per repo (#265): the top authors by commit count,
/// enough to answer "is this one of my repos?" without bloating the registry.
const MAX_CONTRIBUTORS: usize = 100;

/// The repo's git authors, most commits first (`git shortlog -sne HEAD`),
/// capped at [`MAX_CONTRIBUTORS`]. Best-effort: a non-git repo or any failure
/// yields none.
fn git_contributors(root: &Path) -> Vec<glyphtrail_core::Contributor> {
    use glyphtrail_core::Contributor;
    let Some(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["shortlog", "-sne", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return Vec::new();
    };
    let mut contributors = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((count, who)) = line.trim_start().split_once('\t') else {
            continue;
        };
        let Ok(commits) = count.trim().parse::<u32>() else {
            continue;
        };
        let who = who.trim();
        let (name, email) = match (who.rfind('<'), who.rfind('>')) {
            (Some(lt), Some(gt)) if gt > lt => {
                (who[..lt].trim().to_string(), who[lt + 1..gt].to_string())
            }
            _ => (who.to_string(), String::new()),
        };
        contributors.push(Contributor {
            name,
            email,
            commits,
        });
        if contributors.len() >= MAX_CONTRIBUTORS {
            break;
        }
    }
    contributors
}

/// The user's configured git email (`git config --get user.email`), for
/// `repo list --mine` (#265).
fn git_user_email() -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--get", "user.email"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!email.is_empty()).then_some(email)
}

/// One-line outcome message for a [`Resolution`] from registering `name` at
/// `root`.
fn describe_record(res: &Resolution, name: &str, root: &Path) -> String {
    match res {
        Resolution::Added => format!("registered '{name}' -> {}", root.display()),
        Resolution::Updated => format!("updated '{name}' -> {}", root.display()),
        Resolution::SameRepo {
            into,
            new_path: true,
        } => format!(
            "'{name}' is the same repo as '{into}'; recorded extra path {}",
            root.display()
        ),
        Resolution::SameRepo {
            into,
            new_path: false,
        } => format!("'{name}' is already registered as '{into}'"),
    }
}

/// Options for [`scan`], mirroring the `repo scan` flags.
struct ScanOpts {
    analyze: bool,
    update: bool,
    all: bool,
    recursive: bool,
    hidden: bool,
}

/// Version-control markers that mark a directory as a repository root.
const VCS_MARKERS: [&str; 4] = [".git", ".svn", ".bzr", ".hg"];

/// Whether `dir` is a repository root (holds a VCS marker). `.git` may be a file
/// (worktrees, submodules) or a directory, so existence is enough.
fn is_repo_root(dir: &Path) -> bool {
    VCS_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Whether `root` already has a built index (`.glyphtrail/ladybug`). Cheap
/// filesystem check, so unindexed repos can be skipped without shelling out to
/// git for their identities.
fn is_indexed(root: &Path) -> bool {
    glyphtrail_core::config::RepoPaths::new(root)
        .index_dir
        .join("ladybug")
        .exists()
}

/// Collect repository roots under `start` (depth-first), ticking `pb` per
/// directory entered so a slow (e.g. network) walk shows progress. A repo root
/// is a boundary unless `recursive` is set (then nested repos / submodules are
/// also found). Dot-directories are skipped unless `hidden` is set, so tool
/// caches and worktrees (`.git`, `.cache`, `.claude/worktree`) aren't scanned.
/// Symlinked directories are never followed, so cycles can't trap the walk.
fn find_repo_roots(
    start: &Path,
    recursive: bool,
    hidden: bool,
    out: &mut Vec<PathBuf>,
    pb: &ProgressBar,
) {
    pb.inc(1);
    pb.set_message(format!("{} repos", out.len()));
    if is_repo_root(start) {
        out.push(start.to_path_buf());
        if !recursive {
            return; // boundary: don't descend into the repo
        }
    }
    let Ok(entries) = std::fs::read_dir(start) else {
        return; // unreadable dir (permissions) — skip, don't abort the scan
    };
    for entry in entries.flatten() {
        // Only descend into real directories; skip symlinks to avoid cycles.
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {}
            _ => continue,
        }
        // Skip dot-directories unless asked; their `.git` etc. is still detected
        // via `is_repo_root` on the parent, so real repos aren't missed.
        if !hidden
            && entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        find_repo_roots(&entry.path(), recursive, hidden, out, pb);
    }
}

/// A steady-ticking spinner on stderr, auto-hidden when stderr isn't a TTY.
fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {prefix}: {pos} dirs, {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    pb.set_prefix(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb
}

/// Recursively register repositories under `dir` (#263). The walk and per-repo
/// work both show progress, since either can be slow on a network drive.
///
/// By default only already-indexed repos are registered, so a repo whose prior
/// registration was lost (e.g. to a stuck lock) is recovered without pulling in
/// every stray checkout. `--analyze` indexes each repo as it's found (so it then
/// qualifies); `--all` registers everything regardless.
/// Recursively discover repositories under `dir`, analyze each, and register
/// them — the multi-repo form of `glyphtrail analyze` (#386). Equivalent to
/// `repo scan --analyze --all --recursive`: it descends into nested repos and,
/// because each is indexed, registers them all.
pub fn analyze_tree(dir: &Path, update: bool, hidden: bool) -> Result<()> {
    scan(
        &registry_path()?,
        dir,
        ScanOpts {
            analyze: true,
            update,
            all: true,
            recursive: true,
            hidden,
        },
    )
}

fn scan(registry_path: &Path, dir: &Path, opts: ScanOpts) -> Result<()> {
    let root = dir
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", dir.display()))?;

    let walk = spinner(&format!("scanning {}", root.display()));
    let mut roots = Vec::new();
    find_repo_roots(&root, opts.recursive, opts.hidden, &mut roots, &walk);
    walk.finish_and_clear();
    // Drop repos excluded by ~/.glyphtrailignore (#269) before doing any work.
    let before = roots.len();
    roots.retain(|r| !super::analyze::is_path_excluded(r));
    let excluded = before - roots.len();
    if roots.is_empty() {
        println!("no repositories found under {}", root.display());
        return Ok(());
    }
    print!(
        "found {} repositories under {}",
        roots.len(),
        root.display()
    );
    if excluded > 0 {
        print!(" ({excluded} excluded by ~/.glyphtrailignore)");
    }
    println!();

    let (mut registered, mut merged, mut queued, mut analyzed, mut skipped, mut failed) =
        (0u32, 0, 0, 0, 0, 0);
    let bar = ProgressBar::new(roots.len() as u64);
    bar.set_style(
        ProgressStyle::with_template("{spinner:.cyan} [{pos}/{len}] {wide_msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    for repo in &roots {
        let name = repo
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into());
        bar.set_message(name.clone());

        // Analyze first when asked, so a freshly-indexed repo then qualifies for
        // registration below. Suspend the scan bar for the whole analysis so its
        // own parse/resolve bars own the terminal — otherwise the two fight and
        // flicker.
        if opts.analyze {
            match bar.suspend(|| super::analyze::run(repo, opts.update)) {
                Ok(o) if o.up_to_date => bar.suspend(|| {
                    println!("{name}: index up to date ({} files)", o.files);
                }),
                Ok(o) => {
                    analyzed += 1;
                    bar.suspend(|| {
                        println!(
                            "{name}: indexed {} files: {} nodes, {} edges",
                            o.files, o.nodes, o.edges
                        );
                    });
                }
                Err(err) => {
                    failed += 1;
                    bar.suspend(|| eprintln!("{name}: analyze failed: {err:#}"));
                }
            }
        }

        // Register only indexed repos unless `--all`. Registration always runs
        // for a qualifying repo even if it was already indexed, recovering one
        // whose earlier registration was lost.
        if !opts.all && !is_indexed(repo) {
            skipped += 1;
            bar.suspend(|| {
                println!("{name}: skipped (no index; run with --analyze or --all)");
            });
            bar.inc(1);
            continue;
        }
        let entry = entry_for(registry_path, repo, Some(name.clone()));
        match Registry::record(registry_path, vec![entry])? {
            RecordOutcome::Applied(res) => {
                match &res[0] {
                    Resolution::SameRepo { .. } => merged += 1,
                    _ => registered += 1,
                }
                let line = describe_record(&res[0], &name, repo);
                bar.suspend(|| println!("{line}"));
            }
            RecordOutcome::Spilled => {
                queued += 1;
                bar.suspend(|| {
                    println!(
                        "registry busy; queued '{}' -> {} (merged on the next run)",
                        name,
                        repo.display()
                    )
                });
            }
        }
        bar.inc(1);
    }
    bar.finish_and_clear();

    print!("scan: {registered} registered");
    if merged > 0 {
        print!(", {merged} merged");
    }
    if queued > 0 {
        print!(", {queued} queued");
    }
    if skipped > 0 {
        print!(", {skipped} skipped");
    }
    if opts.analyze {
        print!(", {analyzed} analyzed, {failed} failed");
    }
    println!();
    Ok(())
}

/// The URLs of a repo's git remotes, de-duplicated. Best-effort: a non-git
/// directory or any git failure yields no URLs (the repo just gets no stable
/// ids — its name stays the handle).
fn git_remote_urls(root: &Path) -> Vec<String> {
    let remotes = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("remote")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut urls = Vec::new();
    for name in remotes {
        if let Some(out) = Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("remote")
            .arg("get-url")
            .arg(&name)
            .output()
            .ok()
            .filter(|o| o.status.success())
        {
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !url.is_empty() && !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    urls
}

/// Render how long an entry has been missing, e.g. ` for 5d`. Empty when the
/// stamp is absent (just observed missing this run).
fn missing_for(missing_since: Option<i64>) -> String {
    let Some(since) = missing_since else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(since);
    let days = (now - since).max(0) / 86_400;
    format!(" for {days}d")
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn scratch() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("glyphtrail-scan-{nanos}"));
        // alpha is a git repo; sub/beta an svn repo; alpha/vendor/gamma a nested
        // git repo; notrepo holds no VCS marker; .hidden/delta lives under a
        // dot-directory.
        for sub in [
            "alpha/.git",
            "sub/beta/.svn",
            "alpha/vendor/gamma/.git",
            "notrepo/src",
            ".hidden/delta/.git",
        ] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        dir
    }

    fn names_of(roots: &[PathBuf]) -> Vec<String> {
        roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn scan_stops_at_repo_boundaries_and_skips_dot_dirs() {
        let dir = scratch();
        let mut roots = Vec::new();
        find_repo_roots(&dir, false, false, &mut roots, &ProgressBar::hidden());
        let names = names_of(&roots);
        check!(names.contains(&"alpha".to_string()));
        check!(names.contains(&"beta".to_string()));
        check!(!names.contains(&"gamma".to_string())); // inside alpha (a boundary)
        check!(!names.contains(&"delta".to_string())); // inside a dot-directory
        check!(roots.len() == 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_recursive_descends_into_repos() {
        let dir = scratch();
        let mut roots = Vec::new();
        find_repo_roots(&dir, true, false, &mut roots, &ProgressBar::hidden());
        check!(names_of(&roots).contains(&"gamma".to_string())); // submodule now found
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_hidden_descends_into_dot_dirs() {
        let dir = scratch();
        let mut roots = Vec::new();
        find_repo_roots(&dir, false, true, &mut roots, &ProgressBar::hidden());
        check!(names_of(&roots).contains(&"delta".to_string())); // dot-dir repo found
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_repo_root_recognizes_markers() {
        let dir = scratch();
        check!(is_repo_root(&dir.join("alpha")));
        check!(is_repo_root(&dir.join("sub/beta")));
        check!(!is_repo_root(&dir.join("notrepo")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
