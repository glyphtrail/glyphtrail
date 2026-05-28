use std::path::Path;

use anyhow::{bail, Result};
use clap::Subcommand;
use meridian_core::config::RepoPaths;
use meridian_core::{EdgeKind, Node};
use meridian_store::SqliteStore;
use serde::Serialize;

#[derive(Subcommand)]
pub enum QueryCmd {
    /// Show definition(s) matching a name.
    Def { name: String },
    /// Who calls this symbol.
    Callers { name: String },
    /// What this symbol calls.
    Callees { name: String },
    /// Direct neighbours in any direction.
    Neighbors { name: String },
    /// Full-text search over names and doc comments.
    Search { text: String },
    /// Transitive set of symbols affected if this one changes.
    Impact {
        name: String,
        #[arg(long, default_value_t = 5)]
        depth: usize,
    },
}

#[derive(Serialize)]
struct NeighborOut {
    node: Node,
    edge: String,
    confidence: String,
}

fn resolve_one(store: &SqliteStore, name: &str) -> Result<Node> {
    let matches = store.find_by_name(name)?;
    match matches.into_iter().next() {
        Some(n) => Ok(n),
        None => bail!("no symbol named '{name}' in the index"),
    }
}

fn print_nodes(nodes: &[Node], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(nodes)?);
        return Ok(());
    }
    if nodes.is_empty() {
        println!("(none)");
    }
    for n in nodes {
        let loc = n
            .span
            .map(|s| format!("{}:{}", n.file, s.start_line))
            .unwrap_or_else(|| n.file.clone());
        println!("[{}] {} ({})", n.kind.as_str(), n.qualified_name, loc);
        if let Some(doc) = &n.doc {
            let first = doc.lines().next().unwrap_or("");
            println!("    {}", first);
        }
    }
    Ok(())
}

fn print_neighbors(items: &[(Node, EdgeKind, meridian_core::Confidence)], json: bool) -> Result<()> {
    if json {
        let out: Vec<NeighborOut> = items
            .iter()
            .map(|(n, e, c)| NeighborOut {
                node: n.clone(),
                edge: e.as_str().to_string(),
                confidence: c.as_str().to_string(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("(none)");
    }
    for (n, e, c) in items {
        let loc = n
            .span
            .map(|s| format!("{}:{}", n.file, s.start_line))
            .unwrap_or_else(|| n.file.clone());
        println!(
            "{:>10} {} ({}) [{}]",
            e.as_str(),
            n.qualified_name,
            loc,
            c.as_str()
        );
    }
    Ok(())
}

pub fn run(repo: &Path, cmd: QueryCmd, json: bool) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!("no index found at {} — run `meridian analyze` first", paths.db_path.display());
    }
    let store = SqliteStore::open(&paths.db_path)?;

    match cmd {
        QueryCmd::Def { name } => {
            let nodes = store.find_by_name(&name)?;
            print_nodes(&nodes, json)?;
        }
        QueryCmd::Callers { name } => {
            let n = resolve_one(&store, &name)?;
            let items = store.neighbors(&n.id.0, Some(EdgeKind::Calls), false)?;
            print_neighbors(&items, json)?;
        }
        QueryCmd::Callees { name } => {
            let n = resolve_one(&store, &name)?;
            let items = store.neighbors(&n.id.0, Some(EdgeKind::Calls), true)?;
            print_neighbors(&items, json)?;
        }
        QueryCmd::Neighbors { name } => {
            let n = resolve_one(&store, &name)?;
            let mut items = store.neighbors(&n.id.0, None, true)?;
            items.extend(store.neighbors(&n.id.0, None, false)?);
            print_neighbors(&items, json)?;
        }
        QueryCmd::Search { text } => {
            let nodes = store.search(&text, 50)?;
            print_nodes(&nodes, json)?;
        }
        QueryCmd::Impact { name, depth } => {
            let n = resolve_one(&store, &name)?;
            // Callers (transitively) are what breaks if this symbol changes.
            let nodes = store.reachable(&n.id.0, EdgeKind::Calls, false, depth)?;
            print_nodes(&nodes, json)?;
        }
    }
    Ok(())
}
