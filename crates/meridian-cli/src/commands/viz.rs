use std::path::Path;

use anyhow::{Result, bail};
use meridian_core::config::RepoPaths;
use meridian_core::{ImpactPolicy, ImpactReport, Node, NodeId, OperationKey};
use meridian_store::SqliteStore;
use meridian_viz::ImpactView;

pub fn run(
    repo: &Path,
    output: &Path,
    limit: usize,
    impact: Option<&str>,
    cross_boundary: bool,
    depth: usize,
) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            paths.db_path.display()
        );
    }
    let store = SqliteStore::open(&paths.db_path)?;

    if let Some(seed) = impact {
        return run_impact(&store, output, seed, cross_boundary, depth);
    }

    let (nodes, edges) = store.export_graph(limit)?;
    let ops = store.all_operations()?;
    let html = meridian_viz::static_html(&nodes, &edges, &ops);
    std::fs::write(output, html)?;
    println!(
        "Wrote {} ({} nodes, {} edges)",
        output.display(),
        nodes.len(),
        edges.len()
    );
    Ok(())
}

/// Render the impacted subgraph for `seed`: seeds + impacted nodes and the edges
/// among them, highlighted by class / distance / confidence.
fn run_impact(
    store: &SqliteStore,
    output: &Path,
    seed: &str,
    cross_boundary: bool,
    depth: usize,
) -> Result<()> {
    let seeds: Vec<String> = store
        .find_by_name(seed)?
        .into_iter()
        .map(|n| n.id.0)
        .collect();
    if seeds.is_empty() {
        bail!("no symbol named '{seed}' in the index");
    }
    let policy = if cross_boundary {
        ImpactPolicy::cross_boundary(depth)
    } else {
        ImpactPolicy::in_process(depth)
    };
    let seed_ids: Vec<NodeId> = seeds.iter().cloned().map(NodeId).collect();
    let items = store.classify_impact(&seed_ids, &policy)?;
    let report = ImpactReport::new(items, Vec::new(), Vec::new());

    // Focused subgraph: seeds + every impacted node, and the edges among them.
    let mut ids = seeds.clone();
    ids.extend(report.items.iter().map(|i| i.id.clone()));
    let (nodes, edges) = store.subgraph(&ids)?;

    let view = ImpactView {
        seeds: &seeds,
        report: &report,
    };
    let ops = ops_for(store, &nodes)?;
    let html = meridian_viz::static_html_impact(&nodes, &edges, &ops, &view);
    std::fs::write(output, html)?;
    println!(
        "Wrote {} (impact of '{seed}': {} nodes, {} edges)",
        output.display(),
        nodes.len(),
        edges.len()
    );
    Ok(())
}

/// API operation keys for the subgraph's nodes, so endpoints/clients keep their
/// protocol/method/path annotations in the impact view.
fn ops_for(store: &SqliteStore, nodes: &[Node]) -> Result<Vec<(NodeId, OperationKey)>> {
    let ids: std::collections::HashSet<&str> = nodes.iter().map(|n| n.id.0.as_str()).collect();
    Ok(store
        .all_operations()?
        .into_iter()
        .filter(|(id, _)| ids.contains(id.0.as_str()))
        .collect())
}
