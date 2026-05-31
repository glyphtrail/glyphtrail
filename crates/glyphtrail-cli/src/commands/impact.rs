//! `glyphtrail impact` — blast radius from a symbol, file, or change set (#73).
//!
//! Ties the traversal engine (#69), change-set seeding (#70), cross-boundary
//! propagation (#71) and classification (#72) into one report, rendered as text
//! or a stable JSON `ImpactReport`.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use globset::{Glob, GlobSetBuilder};
use glyphtrail_core::config::{Config, RepoPaths};
use glyphtrail_core::{
    ClassifiedItem, Confidence, FederatedReport, ImpactClass, ImpactPolicy, ImpactReport,
};
use glyphtrail_store::{
    ChangeSpec, FederationScope, GraphStore, SeedSet, SeedSpec, changed_files, federated_impact,
    seed_nodes,
};

use crate::commands::backend;

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

    /// Extend the blast radius into other indexed repos that depend on this one,
    /// federating over the whole registry. Requires this repo to be registered.
    #[arg(long)]
    pub downstream: bool,
    /// Like --downstream, but scope the federation to a named group.
    #[arg(long)]
    pub group: Option<String>,

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
    let format = if args.json && args.format == Format::Text {
        Format::Json
    } else {
        args.format
    };

    // Cross-repo mode: federate over the registry (or a named group) so the
    // blast radius reaches downstream repos that depend on this one (#222).
    if args.downstream || args.group.is_some() {
        return run_federated(&args, format);
    }

    let paths = RepoPaths::new(&args.repo);
    let store = backend::open_existing(&paths)?;

    let seed_set = resolve_seeds(store.as_ref(), &args)?;
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
            policy.edges = glyphtrail_core::edge_rules(&tokens).map_err(|e| anyhow!(e))?;
        }
        if let Some(mc) = &args.min_confidence {
            policy.min_confidence =
                glyphtrail_core::parse_confidence(mc).map_err(|e| anyhow!(e))?;
        }
        let mut classified = store.classify_impact(&seed_set.seeds, &policy)?;
        // Apply config-tunable test classification (#131) before the report
        // recomputes its summary, so configured test files count as tests.
        let cfg = Config::load(&args.repo)?;
        retag_configured_tests(&mut classified, &cfg.impact.test_globs)?;
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

/// Build the traversal policy from the shared impact arguments.
fn build_policy(args: &ImpactArgs) -> Result<ImpactPolicy> {
    let mut policy = if args.cross_boundary {
        ImpactPolicy::cross_boundary(args.depth)
    } else {
        ImpactPolicy::in_process(args.depth)
    };
    if let Some(edges) = &args.edges {
        let tokens: Vec<&str> = edges.iter().map(String::as_str).collect();
        policy.edges = glyphtrail_core::edge_rules(&tokens).map_err(|e| anyhow!(e))?;
    }
    if let Some(mc) = &args.min_confidence {
        policy.min_confidence = glyphtrail_core::parse_confidence(mc).map_err(|e| anyhow!(e))?;
    }
    Ok(policy)
}

/// Build the seed spec from the impact arguments, with the same precedence as
/// the single-repo path: a symbol name, else a git change set.
fn seed_spec(args: &ImpactArgs) -> Result<SeedSpec> {
    if let Some(name) = &args.name {
        return Ok(SeedSpec::Name(name.clone()));
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
    Ok(SeedSpec::Change(spec))
}

/// Federated blast radius (#222): seed in the current repo and traverse into
/// downstream repos across the cross-repo link table. Scope is the whole
/// registry, or a named group via `--group`. The heavy lifting lives in
/// [`federated_impact`]; here we just shape the inputs, apply the origin repo's
/// configured test globs, and render.
fn run_federated(args: &ImpactArgs, format: Format) -> Result<()> {
    let scope = match &args.group {
        Some(g) => FederationScope::Group(g.clone()),
        None => FederationScope::Registry,
    };
    let mut report = federated_impact(&args.repo, &scope, seed_spec(args)?, &build_policy(args)?)?;

    // Apply the origin repo's configured test globs (#131), then rebuild so the
    // summary counts the re-tagged tests.
    let cfg = Config::load(&args.repo)?;
    if !cfg.impact.test_globs.is_empty() {
        for r in &mut report.repos {
            retag_configured_tests(&mut r.items, &cfg.impact.test_globs)?;
        }
        report = FederatedReport::new(
            std::mem::take(&mut report.repos),
            std::mem::take(&mut report.crate_level),
        );
    }
    emit_federated(&report, format)
}

fn emit_federated(report: &FederatedReport, format: Format) -> Result<()> {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(report)?),
        Format::Yaml => print!("{}", serde_norway::to_string(report)?),
        Format::Md => print!("{}", federated_markdown(report)),
        Format::Text => print_federated_text(report),
    }
    Ok(())
}

fn print_federated_text(report: &FederatedReport) {
    let s = &report.summary;
    println!(
        "blast radius: {} symbols across {} repo(s) ({} downstream); {} tests, {} API, {} cross-boundary",
        s.total,
        report.repos.len(),
        report.downstream_repos(),
        s.tests,
        s.api,
        s.cross_boundary
    );
    for r in &report.repos {
        let tag = if r.origin { "origin" } else { "downstream" };
        println!("\n== {} ({tag}) ==", r.repo);
        if r.items.is_empty() {
            println!("  (no impacted symbols)");
        }
        for i in &r.items {
            print_item(i);
        }
    }
    if !report.crate_level.is_empty() {
        println!("\npotentially affected (crate-level, unresolved symbol):");
        for h in &report.crate_level {
            println!("  {} {} (via {})", h.repo, h.file, h.via);
        }
    }
}

fn federated_markdown(report: &FederatedReport) -> String {
    let s = &report.summary;
    let mut md = String::from("## Cross-repo impact analysis\n\n");
    md.push_str(&format!(
        "**Blast radius:** {} symbols across {} repo(s) · {} tests · {} API · {} cross-boundary · max distance {}\n",
        s.total,
        report.repos.len(),
        s.tests,
        s.api,
        s.cross_boundary,
        s.max_distance
    ));
    for r in &report.repos {
        let tag = if r.origin { "origin" } else { "downstream" };
        md.push_str(&format!("\n### {} ({tag})\n\n", r.repo));
        if r.items.is_empty() {
            md.push_str("_(no impacted symbols)_\n");
        }
        for i in &r.items {
            let loc = match i.line {
                Some(l) => format!("{}:{}", i.file, l),
                None => i.file.clone(),
            };
            md.push_str(&format!(
                "- `{}` ({}) — d{}\n",
                i.qualified_name, loc, i.distance
            ));
        }
    }
    if !report.crate_level.is_empty() {
        md.push_str("\n### Potentially affected (crate-level, unresolved symbol)\n\n");
        for h in &report.crate_level {
            md.push_str(&format!("- {} `{}` (via {})\n", h.repo, h.file, h.via));
        }
    }
    md
}

/// Re-tag any impacted item whose file matches a configured `test_globs`
/// pattern as a [`ImpactClass::Test`] (#131). A user-declared test path wins
/// over the built-in class. No-op when `globs` is empty, so default behavior is
/// unchanged.
fn retag_configured_tests(items: &mut [ClassifiedItem], globs: &[String]) -> Result<()> {
    if globs.is_empty() {
        return Ok(());
    }
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        builder
            .add(Glob::new(g).with_context(|| format!("invalid impact.test_globs entry {g:?}"))?);
    }
    let set = builder
        .build()
        .context("building impact.test_globs matcher")?;
    for item in items.iter_mut() {
        if set.is_match(&item.file) {
            item.class = ImpactClass::Test;
        }
    }
    Ok(())
}

fn resolve_seeds(store: &dyn GraphStore, args: &ImpactArgs) -> Result<SeedSet> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use glyphtrail_core::NodeKind;

    fn item(file: &str, class: ImpactClass) -> ClassifiedItem {
        ClassifiedItem {
            id: "x".into(),
            name: "n".into(),
            qualified_name: "n".into(),
            kind: NodeKind::Function,
            file: file.into(),
            line: None,
            class,
            distance: 1,
            min_confidence: Confidence::Extracted,
            cross_boundary: false,
            path: Vec::new(),
        }
    }

    // #131: files matching a configured glob are re-tagged as tests; others keep
    // their built-in class.
    #[test]
    fn test_globs_retag_matching_files() {
        let mut items = vec![
            item("it/login_it.rs", ImpactClass::Internal),
            item("src/util.rs", ImpactClass::Internal),
        ];
        retag_configured_tests(&mut items, &["it/**".to_string()]).unwrap();
        check!(items[0].class == ImpactClass::Test);
        check!(items[1].class == ImpactClass::Internal);
    }

    #[test]
    fn empty_globs_change_nothing() {
        let mut items = vec![item("src/util.rs", ImpactClass::Internal)];
        retag_configured_tests(&mut items, &[]).unwrap();
        check!(items[0].class == ImpactClass::Internal);
    }

    #[test]
    fn invalid_glob_is_an_error() {
        let mut items = vec![item("a.rs", ImpactClass::Internal)];
        check!(retag_configured_tests(&mut items, &["[".to_string()]).is_err());
    }
}
