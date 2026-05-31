//! `stratograph story` — narrate a repository's history from its git log via an
//! LLM (#114). History facts are gathered offline and deterministically with
//! `git log`, composed into a single prompt, and sent to a configurable
//! provider (shared with `wiki`). `--dry-run` writes the prompt instead of
//! calling the API, so the git→prompt pipeline is testable without network/keys.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use stratograph_core::{Registry, RegistryEntry, default_registry_path};

use super::llm::{Llm, Provider};

#[derive(Args)]
pub struct StoryArgs {
    /// Repository root.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// LLM provider.
    #[arg(long, value_enum, default_value_t)]
    pub provider: Provider,
    /// Model id (defaults to a sensible per-provider model).
    #[arg(long)]
    pub model: Option<String>,
    /// Override the API base URL (for OpenAI-compatible gateways, e.g. Kilo).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Most recent commits to summarize.
    #[arg(long, default_value_t = 200)]
    pub max_commits: usize,
    /// Output file for the generated story.
    #[arg(long, default_value = "STORY.md")]
    pub output: PathBuf,
    /// Narrate every repository in the global registry as one portfolio story.
    #[arg(long)]
    pub all: bool,
    /// Narrate the named registered repositories (comma-separated) as one
    /// portfolio story. Overrides `--repo`.
    #[arg(long, value_delimiter = ',')]
    pub repos: Option<Vec<String>>,
    /// Write the composed prompt instead of calling the LLM (no network/keys).
    #[arg(long)]
    pub dry_run: bool,
}

/// One commit's facts, newest-first as `git log` emits them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Commit {
    hash: String,
    date: String,
    author: String,
    subject: String,
}

const SYSTEM: &str = "You are a technical writer narrating the evolution of a software project \
from its git history. Use only the provided facts — do not invent commits, features, dates, or \
people. Write engaging, accurate GitHub-flavored Markdown: a short intro, the project's phases \
and milestones inferred from commit themes and dates, notable contributors, and where it stands \
now. Group related commits into themes rather than listing them verbatim.";

pub fn run(args: StoryArgs) -> Result<()> {
    if args.all || args.repos.is_some() {
        return run_portfolio(args);
    }
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", args.repo.display()))?;
    let name = repo
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let commits = git_history(&repo, args.max_commits)?;
    if commits.is_empty() {
        bail!("no commits found in {} (not a git repo?)", repo.display());
    }
    let user = story_prompt(&name, &commits);

    if args.dry_run {
        std::fs::write(
            &args.output,
            format!("# SYSTEM\n{SYSTEM}\n\n# USER\n{user}\n"),
        )
        .with_context(|| format!("cannot write {}", args.output.display()))?;
        println!(
            "wrote {} ({} commits; dry run, no LLM called)",
            args.output.display(),
            commits.len()
        );
        return Ok(());
    }

    let llm = Llm::new(args.provider, args.model, args.base_url)?;
    let md = llm.complete(SYSTEM, &user).context("generating story")?;
    std::fs::write(&args.output, md)
        .with_context(|| format!("cannot write {}", args.output.display()))?;
    println!("wrote {}", args.output.display());
    Ok(())
}

/// Narrate several registered repositories as one portfolio story (#114): the
/// `--all` / `--repos` selection the issue calls for, woven into a single
/// cross-repo history rather than per-repo files.
fn run_portfolio(args: StoryArgs) -> Result<()> {
    let path = default_registry_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))?;
    let registry = Registry::load(&path)?;
    let selected = select_repos(&registry, args.all, &args.repos)?;
    if selected.is_empty() {
        bail!("no repositories registered; use `stratograph repo add`");
    }
    // Gather each repo's history; a missing/empty repo is reported and skipped
    // rather than aborting the whole portfolio.
    let mut histories: Vec<(String, Vec<Commit>)> = Vec::new();
    for e in &selected {
        match git_history(e.active_root(), args.max_commits) {
            Ok(commits) if !commits.is_empty() => histories.push((e.name.clone(), commits)),
            Ok(_) => eprintln!("  {}: no commits, skipped", e.name),
            Err(err) => eprintln!("  {}: {err:#}", e.name),
        }
    }
    if histories.is_empty() {
        bail!("no commit history found across the selected repositories");
    }
    let user = portfolio_prompt(&histories);

    if args.dry_run {
        std::fs::write(
            &args.output,
            format!("# SYSTEM\n{SYSTEM}\n\n# USER\n{user}\n"),
        )
        .with_context(|| format!("cannot write {}", args.output.display()))?;
        println!(
            "wrote {} ({} repositories; dry run, no LLM called)",
            args.output.display(),
            histories.len()
        );
        return Ok(());
    }

    let llm = Llm::new(args.provider, args.model, args.base_url)?;
    let md = llm
        .complete(SYSTEM, &user)
        .context("generating portfolio story")?;
    std::fs::write(&args.output, md)
        .with_context(|| format!("cannot write {}", args.output.display()))?;
    println!(
        "wrote {} ({} repositories)",
        args.output.display(),
        histories.len()
    );
    Ok(())
}

/// Select registry entries: every repo for `all`, else the named ones (erroring
/// on an unknown name).
fn select_repos<'a>(
    registry: &'a Registry,
    all: bool,
    names: &Option<Vec<String>>,
) -> Result<Vec<&'a RegistryEntry>> {
    match (all, names) {
        (true, _) => Ok(registry.repos.iter().collect()),
        (false, Some(ns)) => ns
            .iter()
            .map(|name| {
                registry
                    .get(name)
                    .ok_or_else(|| anyhow!("no repository named '{name}' in the registry"))
            })
            .collect(),
        (false, None) => Ok(registry.repos.iter().collect()),
    }
}

/// The most recent `limit` non-merge commits, newest first. Fields are split on
/// the ASCII unit separator (`\x1f`) so subjects may contain any other byte.
fn git_history(repo: &std::path::Path, limit: usize) -> Result<Vec<Commit>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            "--no-merges",
            "--date=short",
            "--pretty=format:%h\x1f%ad\x1f%an\x1f%s",
            "-n",
        ])
        .arg(limit.to_string())
        .output()
        .context("running git log")?;
    if !out.status.success() {
        bail!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_log(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `%h\x1f%ad\x1f%an\x1f%s` lines into commits.
fn parse_log(text: &str) -> Vec<Commit> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\x1f');
            Some(Commit {
                hash: f.next()?.to_string(),
                date: f.next()?.to_string(),
                author: f.next()?.to_string(),
                subject: f.next()?.to_string(),
            })
        })
        .collect()
}

/// Compose the user prompt: a facts header (span, contributors) plus the commit
/// subjects, newest first.
/// A single repository's history facts block: window span, top contributors and
/// the commit log. Shared by the single-repo and portfolio prompts.
fn repo_block(repo_name: &str, commits: &[Commit]) -> String {
    // `git log` is newest-first, so the last entry is the oldest in this window.
    let newest = &commits[0].date;
    let oldest = &commits[commits.len() - 1].date;

    let mut authors: Vec<(String, usize)> = Vec::new();
    for c in commits {
        match authors.iter_mut().find(|(a, _)| a == &c.author) {
            Some((_, n)) => *n += 1,
            None => authors.push((c.author.clone(), 1)),
        }
    }
    authors.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let contributors = authors
        .iter()
        .take(10)
        .map(|(a, n)| format!("{a} ({n})"))
        .collect::<Vec<_>>()
        .join(", ");

    let log = commits
        .iter()
        .map(|c| format!("- {} {} <{}> {}", c.date, c.hash, c.author, c.subject))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Repository: {repo_name}\nCommits in window: {} (newest {newest}, oldest {oldest})\n\
         Top contributors: {contributors}\n\n\
         Commit log (newest first):\n{log}",
        commits.len()
    )
}

fn story_prompt(repo_name: &str, commits: &[Commit]) -> String {
    format!(
        "{}\n\nWrite a `History` page narrating how {repo_name} evolved over this window.",
        repo_block(repo_name, commits)
    )
}

/// Prompt weaving several repositories' histories into one portfolio narrative.
fn portfolio_prompt(repos: &[(String, Vec<Commit>)]) -> String {
    let blocks = repos
        .iter()
        .map(|(name, commits)| repo_block(name, commits))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    format!(
        "{} repositories from the same portfolio:\n\n{blocks}\n\n\
         Write a unified `History` narrating how these repositories evolved \
         together — shared phases and milestones, how effort shifted between them \
         over time, and notable contributors across the portfolio. Use a section \
         per repository where it helps, but weave one coherent story.",
        repos.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn c(date: &str, author: &str, subject: &str) -> Commit {
        Commit {
            hash: "abc1234".into(),
            date: date.into(),
            author: author.into(),
            subject: subject.into(),
        }
    }

    #[test]
    fn parse_log_splits_unit_separated_fields() {
        let text = "h1\x1f2026-01-01\x1fAda\x1fAdd thing\nh2\x1f2026-01-02\x1fGrace\x1fFix: bug\x1fwith sep";
        let commits = parse_log(text);
        check!(commits.len() == 2);
        check!(commits[0].subject == "Add thing");
        // A subject is everything after the 4th field's separator is ignored
        // (we take exactly four splits); the extra "\x1fwith sep" is dropped.
        check!(commits[1].author == "Grace");
        check!(commits[1].subject == "Fix: bug");
    }

    #[test]
    fn story_prompt_summarizes_span_and_contributors() {
        let commits = vec![
            c("2026-03-01", "Ada", "Add parser"),
            c("2026-02-01", "Ada", "Init"),
            c("2026-01-15", "Grace", "Scaffold"),
        ];
        let p = story_prompt("stratograph", &commits);
        check!(p.contains("Repository: stratograph"));
        check!(p.contains("newest 2026-03-01, oldest 2026-01-15"));
        // Ada has the most commits, so leads the contributor list.
        check!(p.contains("Top contributors: Ada (2), Grace (1)"));
        check!(p.contains("- 2026-03-01 abc1234 <Ada> Add parser"));
    }

    #[test]
    fn portfolio_prompt_weaves_each_repo() {
        let alpha = vec![c("2026-03-01", "Ada", "Add parser")];
        let beta = vec![
            c("2026-02-01", "Grace", "Init beta"),
            c("2026-01-01", "Grace", "Scaffold beta"),
        ];
        let p = portfolio_prompt(&[("alpha".into(), alpha), ("beta".into(), beta)]);
        check!(p.starts_with("2 repositories from the same portfolio:"));
        check!(p.contains("Repository: alpha"));
        check!(p.contains("Repository: beta"));
        check!(p.contains("\n---\n")); // blocks separated
        check!(p.contains("weave one coherent story"));
    }
}
