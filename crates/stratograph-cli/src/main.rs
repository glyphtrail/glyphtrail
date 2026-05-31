#![forbid(unsafe_code)]

mod commands;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "stratograph",
    version,
    about = "Map codebases as semantic and historical graphs: query structure, trace lineage, and discover recurring ideas across time"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Walk a repository, parse it, and build/update the graph.
    Analyze {
        /// Repository root (defaults to the current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Only reparse files whose content changed since the last index.
        #[arg(long)]
        update: bool,
        /// Analyze every repository in the global registry instead of `path`.
        #[arg(long)]
        all: bool,
    },
    /// Query the graph.
    Query {
        #[command(subcommand)]
        query: commands::query::QueryCmd,
        /// Repository root.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Emit YAML instead of text (compact structured output for agents).
        #[arg(long)]
        yaml: bool,
        /// Query every repository in the global registry, tagging each result.
        #[arg(long)]
        all: bool,
        /// Query the named registered repositories (comma-separated), tagging
        /// each result. Overrides `--repo`.
        #[arg(long, value_delimiter = ',')]
        repos: Option<Vec<String>>,
    },
    /// Export a self-contained interactive graph.html.
    Viz {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "graph.html")]
        output: PathBuf,
        #[arg(long, default_value_t = 2000)]
        limit: usize,
        /// Highlight the impact blast radius of this seed symbol.
        #[arg(long)]
        impact: Option<String>,
        /// With --impact, include cross-boundary consumers.
        #[arg(long)]
        cross_boundary: bool,
        /// With --impact, max traversal depth.
        #[arg(long, default_value_t = 5)]
        depth: usize,
    },
    /// Serve an interactive graph explorer over HTTP.
    Serve {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value_t = 7700)]
        port: u16,
    },
    /// Compute the blast radius of a change (symbol, file, or git change set).
    Impact(commands::impact::ImpactArgs),
    /// Report API contract drift: code endpoints vs blessed schema operations.
    Drift(commands::drift::DriftArgs),
    /// Run a raw Cypher query against the graph.
    Cypher {
        /// The Cypher query to run.
        query: String,
        /// Repository root.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Generate a documentation wiki from the graph via an LLM.
    Wiki(commands::wiki::WikiArgs),
    /// Narrate the repository's history from its git log via an LLM.
    Story(commands::story::StoryArgs),
    /// Run a Model Context Protocol server over stdio (for agents/editors).
    Mcp {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Onboard coding agents: write skill + CLAUDE.md/AGENTS.md section + gitignore.
    Setup {
        /// Repository root (defaults to the current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Manage the global repository registry (~/.stratograph/registry.json).
    Repo {
        #[command(subcommand)]
        cmd: commands::repo::RepoCmd,
    },
    /// Manage named groups of repositories (~/.stratograph/groups.json).
    Group {
        #[command(subcommand)]
        cmd: commands::group::GroupCmd,
    },
    /// Show index statistics.
    Status {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Show stats for every repository in the global registry.
        #[arg(long)]
        all: bool,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Emit YAML instead of text (compact structured output for agents).
        #[arg(long)]
        yaml: bool,
    },
    /// Generate a shell completion script for the given shell.
    Completions {
        /// Target shell (bash, zsh, fish, powershell, elvish).
        shell: Shell,
    },
}

fn main() -> anyhow::Result<()> {
    // Logs go to stderr so stdout carries only command output — critical for the
    // MCP stdio transport (JSON-RPC) and for piping JSON/YAML query results.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Analyze { path, update, all } => {
            if all {
                commands::repo::analyze_all(update)
            } else {
                let outcome = commands::analyze::run(&path, update)?;
                if outcome.up_to_date {
                    println!(
                        "Index up to date ({} files); nothing changed.",
                        outcome.files
                    );
                } else {
                    println!(
                        "Indexed {} files: {} nodes, {} edges",
                        outcome.files, outcome.nodes, outcome.edges
                    );
                }
                if !outcome.languages.is_empty() {
                    println!(
                        "Languages: {}",
                        commands::status::format_languages(&outcome.languages)
                    );
                }
                // Hint (read-only): onboarding files are written only by `setup`.
                if !path.join(".claude/skills/stratograph/SKILL.md").exists() {
                    println!("Tip: run `stratograph setup` to onboard coding agents (MCP/CLI).");
                }
                Ok(())
            }
        }
        Command::Query {
            query,
            repo,
            json,
            yaml,
            all,
            repos,
        } => {
            let emit = commands::query::Emit::from_flags(json, yaml);
            if all || repos.is_some() {
                commands::query::run_registry(query, emit, all, repos)
            } else {
                commands::query::run(&repo, query, emit)
            }
        }
        Command::Viz {
            repo,
            output,
            limit,
            impact,
            cross_boundary,
            depth,
        } => commands::viz::run(
            &repo,
            &output,
            limit,
            impact.as_deref(),
            cross_boundary,
            depth,
        ),
        Command::Serve { repo, port } => commands::serve::run(&repo, port),
        Command::Impact(args) => commands::impact::run(args),
        Command::Drift(args) => commands::drift::run(args),
        Command::Cypher { query, repo } => commands::cypher::run(&repo, &query),
        Command::Wiki(args) => commands::wiki::run(args),
        Command::Story(args) => commands::story::run(args),
        Command::Mcp { repo } => stratograph_mcp::serve_stdio(repo),
        Command::Setup { path } => commands::setup::run(&path),
        Command::Repo { cmd } => commands::repo::run(cmd),
        Command::Group { cmd } => commands::group::run(cmd),
        Command::Status {
            repo,
            all,
            json,
            yaml,
        } => {
            if all {
                commands::repo::status_all()
            } else {
                commands::status::run(&repo, commands::query::Emit::from_flags(json, yaml))
            }
        }
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }
    }
}
