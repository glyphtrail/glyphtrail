//! `meridian impact` — blast radius from a symbol, file, or change set (#73).
//!
//! Ties the traversal engine (#69), change-set seeding (#70), cross-boundary
//! propagation (#71) and classification (#72) into one report, rendered as text
//! or a stable JSON `ImpactReport`.

use anyhow::{Result, anyhow, bail};
use clap::{Args, ValueEnum};
use meridian_core::config::RepoPaths;
use meridian_core::{ClassifiedItem, Confidence, ImpactClass, ImpactPolicy, ImpactReport};
use meridian_store::{ChangeSpec, SeedSet, SqliteStore, changed_files, seed_nodes};

/// Exit code used by `--gate` when a change touches the API/contract surface.
const GATE_EXIT_CODE: i32 = 2;

/// Output format for the impact report.
#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Format {
    #[default]
    Text,
    Json,
    /// YAML — compact, low-boilerplate structured output for LLMs/agents.
    Yaml,
    /// Markdown for a PR comment or CI job summary.
    Md,
}

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

    /// Output format: text, json, or md (Markdown for CI / PR comments).
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
    /// Shorthand for `--format json`.
    #[arg(long)]
    pub json: bool,
    /// Exit non-zero (code 2) when the change touches the API/contract surface.
    #[arg(long)]
    pub gate: bool,
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

    let format = if args.json && args.format == Format::Text {
        Format::Json
    } else {
        args.format
    };

    let seed_set = resolve_seeds(&store, &args)?;
    let report = if seed_set.seeds.is_empty() {
        // Still emit (possibly noting removed/unresolved files) rather than error.
        ImpactReport::new(
            Vec::new(),
            seed_set.removed_files,
            seed_set.unresolved_files,
        )
    } else {
        let mut policy = if args.cross_boundary {
            ImpactPolicy::cross_boundary(args.depth)
        } else {
            ImpactPolicy::in_process(args.depth)
        };
        if let Some(edges) = &args.edges {
            let tokens: Vec<&str> = edges.iter().map(String::as_str).collect();
            policy.edges = meridian_core::edge_rules(&tokens).map_err(|e| anyhow!(e))?;
        }
        if let Some(mc) = &args.min_confidence {
            policy.min_confidence = meridian_core::parse_confidence(mc).map_err(|e| anyhow!(e))?;
        }
        let classified = store.classify_impact(&seed_set.seeds, &policy)?;
        ImpactReport::new(
            classified,
            seed_set.removed_files,
            seed_set.unresolved_files,
        )
    };

    emit(&report, format)?;

    // Drift gate: a PR touching the API/contract surface exits non-zero so CI
    // can flag it loudly. Default is report-only.
    if args.gate && report.touches_contract() {
        std::process::exit(GATE_EXIT_CODE);
    }
    Ok(())
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

fn emit(report: &ImpactReport, format: Format) -> Result<()> {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(report)?),
        Format::Yaml => print!("{}", serde_norway::to_string(report)?),
        Format::Md => print!("{}", report.to_markdown()),
        Format::Text => print_text(report),
    }
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
