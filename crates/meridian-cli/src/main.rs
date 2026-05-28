#![forbid(unsafe_code)]

mod commands;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "meridian",
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
    },
    /// Export a self-contained interactive graph.html.
    Viz {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "graph.html")]
        output: PathBuf,
        #[arg(long, default_value_t = 2000)]
        limit: usize,
    },
    /// Serve an interactive graph explorer over HTTP.
    Serve {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value_t = 7700)]
        port: u16,
    },
    /// Run a Model Context Protocol server over stdio (for agents/editors).
    Mcp {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Manage the global repository registry (~/.meridian/registry.json).
    Repo {
        #[command(subcommand)]
        cmd: commands::repo::RepoCmd,
    },
    /// Show index statistics.
    Status {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Show stats for every repository in the global registry.
        #[arg(long)]
        all: bool,
    },
    /// Generate a shell completion script for the given shell.
    Completions {
        /// Target shell (bash, zsh, fish, powershell, elvish).
        shell: Shell,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Analyze { path, update, all } => {
            if all {
                commands::repo::analyze_all(update)
            } else {
                commands::analyze::run(&path, update)
            }
        }
        Command::Query { query, repo, json } => commands::query::run(&repo, query, json),
        Command::Viz {
            repo,
            output,
            limit,
        } => commands::viz::run(&repo, &output, limit),
        Command::Serve { repo, port } => commands::serve::run(&repo, port),
        Command::Mcp { repo } => meridian_mcp::serve_stdio(repo),
        Command::Repo { cmd } => commands::repo::run(cmd),
        Command::Status { repo, all } => {
            if all {
                commands::repo::status_all()
            } else {
                commands::status::run(&repo)
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
