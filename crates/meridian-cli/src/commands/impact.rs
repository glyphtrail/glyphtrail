//! `meridian impact` — blast radius from a symbol, file, or change set (#73).
//!
//! Ties the traversal engine (#69), change-set seeding (#70), cross-boundary
//! propagation (#71) and classification (#72) into one report, rendered as text
//! or a stable JSON `ImpactReport`.

use anyhow::{Result, anyhow, bail};
use clap::Args;
use meridian_core::config::RepoPaths;
use meridian_core::{
    ClassifiedItem, Confidence, EdgeKind, ImpactClass, ImpactPolicy, ImpactReport, Node, classify,
    compute_impact, is_cross_boundary_path,
};
use meridian_store::SqliteStore;

use super::changeset::{ChangeSpec, SeedSet, changed_files, seed_nodes};

#[derive(Args)]
pub struct ImpactArgs {
    /// Seed symbol name (omit when using --file/--since/--staged/--diff/--files).
    pub name: Option<String>,

    /// Repository root.
    #[arg(long, default_value = ".")]
    pub repo: std::path::PathBuf,

    /// Seed every symbol in this repo-relative file.
    #[arg(long)]
    pub file: Option<String>,
    /// Seed symbols changed since a git revision/range (e.g. main, main..HEAD).
    #[arg(long)]
    pub since: Option<String>,
    /// Seed from staged changes.
    #[arg(long)]
    pub staged: bool,
    /// Seed from unstaged working-tree changes.
    #[arg(long)]
    pub diff: bool,
    /// Seed every symbol in these repo-relative files.
    #[arg(long, value_delimiter = ',')]
    pub files: Option<Vec<String>>,

    /// Edge kinds to traverse: any of calls,imports,impl,api (default: all).
    #[arg(long, value_delimiter = ',')]
    pub edges: Option<Vec<String>>,
    /// Max hops from a seed.
    #[arg(long, default_value_t = 5)]
    pub depth: usize,
    /// Minimum path confidence: extracted | inferred (default: inferred = keep all).
    #[arg(long)]
    pub min_confidence: Option<String>,
    /// Include cross-boundary consumers (HANDLES/INVOKES/EXPOSES/MOUNTS).
    #[arg(long)]
    pub cross_boundary: bool,

    /// Emit the JSON ImpactReport instead of text.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: ImpactArgs) -> Result<()> {
    let paths = RepoPaths::new(&args.repo);
    if !paths.db_path.exists() {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            paths.db_path.display()
        );
    }
    let store = SqliteStore::open(&paths.db_path)?;

    let seed_set = resolve_seeds(&store, &args)?;
    if seed_set.seeds.is_empty() {
        // Still emit (possibly noting removed/unresolved files) rather than error.
        let report = ImpactReport::new(
            Vec::new(),
            seed_set.removed_files,
            seed_set.unresolved_files,
        );
        return emit(&report, args.json);
    }

    let mut policy = if args.cross_boundary {
        ImpactPolicy::cross_boundary(args.depth)
    } else {
        ImpactPolicy::in_process(args.depth)
    };
    if let Some(edges) = &args.edges {
        policy.edges = parse_edges(edges)?;
    }
    if let Some(mc) = &args.min_confidence {
        policy.min_confidence = parse_confidence(mc)?;
    }

    let items = compute_impact(&seed_set.seeds, &policy, &store);

    // Resolve + classify each impacted node.
    let mut classified = Vec::with_capacity(items.len());
    for it in items {
        let Some(node) = store.get_node(&it.node.0)? else {
            continue; // node vanished between traversal and lookup; skip
        };
        classified.push(to_classified(
            &node,
            &it.path,
            it.distance,
            it.min_confidence,
        ));
    }

    let report = ImpactReport::new(
        classified,
        seed_set.removed_files,
        seed_set.unresolved_files,
    );
    emit(&report, args.json)
}

fn resolve_seeds(store: &SqliteStore, args: &ImpactArgs) -> Result<SeedSet> {
    // Change-set seed modes are mutually exclusive with a symbol name; the first
    // set one wins, in a documented precedence.
    if let Some(name) = &args.name {
        let nodes = store.find_by_name(name)?;
        if nodes.is_empty() {
            bail!("no symbol named '{name}' in the index");
        }
        return Ok(SeedSet {
            seeds: nodes.into_iter().map(|n| n.id).collect(),
            ..Default::default()
        });
    }
    let spec = if let Some(f) = &args.file {
        ChangeSpec::Files(vec![f.clone()])
    } else if let Some(fs) = &args.files {
        ChangeSpec::Files(fs.clone())
    } else if let Some(rev) = &args.since {
        ChangeSpec::Since(rev.clone())
    } else if args.staged {
        ChangeSpec::Staged
    } else if args.diff {
        ChangeSpec::WorkingTree
    } else {
        bail!("provide a symbol name or one of --file/--files/--since/--staged/--diff");
    };
    let files = changed_files(&args.repo, &spec)?;
    seed_nodes(store, &files)
}

fn to_classified(
    node: &Node,
    path: &[EdgeKind],
    distance: usize,
    min_confidence: Confidence,
) -> ClassifiedItem {
    ClassifiedItem {
        id: node.id.0.clone(),
        name: node.name.clone(),
        qualified_name: node.qualified_name.clone(),
        kind: node.kind,
        file: node.file.clone(),
        line: node.span.map(|s| s.start_line),
        class: classify(node.kind, &node.file, &node.qualified_name),
        distance,
        min_confidence,
        cross_boundary: is_cross_boundary_path(path),
        path: path.iter().map(|k| k.as_str().to_string()).collect(),
    }
}

fn parse_edges(tokens: &[String]) -> Result<Vec<meridian_core::EdgeRule>> {
    use meridian_core::EdgeRule;
    let mut rules = Vec::new();
    for t in tokens {
        match t.as_str() {
            "calls" => rules.push(EdgeRule::incoming(EdgeKind::Calls)),
            "imports" => rules.push(EdgeRule::incoming(EdgeKind::Imports)),
            "impl" => {
                rules.push(EdgeRule::incoming(EdgeKind::Implements));
                rules.push(EdgeRule::incoming(EdgeKind::Extends));
            }
            "api" => {
                rules.push(EdgeRule::outgoing(EdgeKind::Handles));
                rules.push(EdgeRule::outgoing(EdgeKind::Exposes));
                rules.push(EdgeRule::incoming(EdgeKind::Invokes));
                rules.push(EdgeRule::incoming(EdgeKind::Mounts));
            }
            other => bail!("unknown edge set '{other}' (expected calls, imports, impl, api)"),
        }
    }
    Ok(rules)
}

fn parse_confidence(s: &str) -> Result<Confidence> {
    match s.to_ascii_lowercase().as_str() {
        "extracted" => Ok(Confidence::Extracted),
        "inferred" => Ok(Confidence::Inferred),
        other => Err(anyhow!(
            "unknown confidence '{other}' (expected extracted or inferred)"
        )),
    }
}

fn emit(report: &ImpactReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    print_text(report);
    Ok(())
}

fn print_text(report: &ImpactReport) {
    println!("{}", report.headline());
    if report.summary.max_distance > 0 {
        println!("max distance: {}", report.summary.max_distance);
    }

    // Cross-boundary consumers (reached across the wire) are listed once, here;
    // every other item is grouped by class below. Partitioning keeps each item
    // in exactly one section.
    let cb: Vec<&ClassifiedItem> = report.items.iter().filter(|i| i.cross_boundary).collect();
    if !cb.is_empty() {
        println!("\ncross-boundary consumers:");
        for i in cb {
            print_item(i);
        }
    }
    for (label, class) in [
        ("tests", ImpactClass::Test),
        ("API surface", ImpactClass::Api),
        ("entrypoints", ImpactClass::Entrypoint),
        ("internal", ImpactClass::Internal),
    ] {
        let group: Vec<&ClassifiedItem> = report
            .items
            .iter()
            .filter(|i| i.class == class && !i.cross_boundary)
            .collect();
        if group.is_empty() {
            continue;
        }
        println!("\n{label}:");
        for i in group {
            print_item(i);
        }
    }
    if !report.removed_files.is_empty() {
        println!("\nremoved files (former dependents may be affected):");
        for f in &report.removed_files {
            println!("  {f}");
        }
    }
    if !report.unresolved_files.is_empty() {
        println!("\nchanged files with no indexed symbol:");
        for f in &report.unresolved_files {
            println!("  {f}");
        }
    }
}

fn print_item(i: &ClassifiedItem) {
    let loc = match i.line {
        Some(l) => format!("{}:{}", i.file, l),
        None => i.file.clone(),
    };
    let path = if i.path.is_empty() {
        String::new()
    } else {
        format!(" [{}]", i.path.join("→"))
    };
    let conf = if i.min_confidence == Confidence::Inferred {
        " ~inferred"
    } else {
        ""
    };
    println!("  {} ({loc}) d{}{conf}{path}", i.qualified_name, i.distance);
}
