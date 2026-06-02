use std::path::Path;

use anyhow::Result;
use glyphtrail_analyze::Staleness;
use glyphtrail_core::config::{Config, RepoPaths};
use glyphtrail_core::{Registry, count_unresolved_links, default_registry_path};
use glyphtrail_store::Stats;
use serde_json::{Value, json};

use crate::commands::backend;
use crate::commands::query::{Emit, print_value};
use crate::commands::setup;

pub fn run(repo: &Path, emit: Emit) -> Result<()> {
    let paths = RepoPaths::new(repo);
    let store = backend::open_existing(&paths)?;
    let s = store.stats()?;
    let location = backend::location(&paths).display().to_string();
    // Whether the index still reflects the working tree (#313). Reported inline
    // rather than as the stderr note other read commands use.
    let staleness = glyphtrail_analyze::index_staleness(repo, store.as_ref());
    // The installed agent-onboarding version, if any (None when not onboarded).
    let skill = setup::installed_version(repo);
    // Configured cross-repo `[[links]]` + how many are dead (#418), so an agent
    // sees that links exist and whether to trust them.
    let links = link_health(repo);
    match emit {
        Emit::Text => print_text(&location, &s, &staleness, skill, links),
        _ => print_value(&stats_value(&location, &s, &staleness, skill, links), emit)?,
    }
    Ok(())
}

/// `(configured, unresolved)` cross-repo link hints for this repo (#418), or
/// `None` when none are declared. Validates each hint's repo against the
/// registry; when the registry can't be loaded, reports 0 unresolved.
fn link_health(repo: &Path) -> Option<(usize, usize)> {
    let links = Config::load(repo).ok()?.links;
    if links.is_empty() {
        return None;
    }
    let Some(registry) = default_registry_path().and_then(|p| Registry::load(&p).ok()) else {
        return Some((links.len(), 0));
    };
    let owner_root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let owner_name = registry
        .name_at_root(&owner_root)
        .unwrap_or(".")
        .to_string();
    let unresolved = count_unresolved_links(&links, &owner_name, &owner_root, &registry);
    Some((links.len(), unresolved))
}

fn print_text(
    location: &str,
    s: &Stats,
    staleness: &Staleness,
    skill: Option<u32>,
    links: Option<(usize, usize)>,
) {
    println!("index:  {location}");
    println!("files:  {}", s.files);
    println!("nodes:  {}", s.nodes);
    println!("edges:  {}", s.edges);
    if !s.languages.is_empty() {
        println!("langs:  {}", format_languages(&s.languages));
    }
    if let Some((configured, unresolved)) = links {
        match unresolved {
            0 => println!("links:  {configured} cross-repo"),
            k => println!(
                "links:  {configured} cross-repo ({k} unresolved — `glyphtrail repo link list`)"
            ),
        }
    }
    match staleness {
        Staleness::Fresh => println!("status: up to date"),
        Staleness::Stale(why) => {
            println!("status: STALE — {why}; run `glyphtrail analyze` to refresh")
        }
        // Indeterminate (dirty/non-git index): say nothing rather than guess.
        Staleness::Unknown => {}
    }
    match skill {
        Some(v) if v < setup::SKILL_VERSION => println!(
            "skill:  v{v} (outdated — bundled v{}; run `glyphtrail setup --local`)",
            setup::SKILL_VERSION
        ),
        Some(v) => println!("skill:  v{v}"),
        // Not onboarded: nothing to report.
        None => {}
    }
}

/// Index statistics as a structured value for `--json` / `--yaml` (#109).
fn stats_value(
    location: &str,
    s: &Stats,
    staleness: &Staleness,
    skill: Option<u32>,
    links: Option<(usize, usize)>,
) -> Value {
    let languages: serde_json::Map<String, Value> = s
        .languages
        .iter()
        .map(|(lang, n)| (lang.clone(), json!(n)))
        .collect();
    let (freshness, reason) = match staleness {
        Staleness::Fresh => ("fresh", None),
        Staleness::Stale(why) => ("stale", Some(why.clone())),
        Staleness::Unknown => ("unknown", None),
    };
    json!({
        "index": location,
        "files": s.files,
        "nodes": s.nodes,
        "edges": s.edges,
        "languages": languages,
        "freshness": freshness,
        "stale": staleness.is_stale(),
        "stale_reason": reason,
        "skill_version": skill,
        "bundled_skill_version": setup::SKILL_VERSION,
        "skill_outdated": skill.is_some_and(|v| v < setup::SKILL_VERSION),
        "links": links.map(|(configured, unresolved)| json!({
            "configured": configured,
            "unresolved": unresolved,
        })),
    })
}

/// Render per-language file counts as `rust 12, python 3`, descending.
pub fn format_languages(languages: &[(String, usize)]) -> String {
    languages
        .iter()
        .map(|(lang, n)| format!("{lang} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn stats_value_is_structured() {
        let s = Stats {
            files: 3,
            nodes: 42,
            edges: 100,
            languages: vec![("rust".into(), 2), ("python".into(), 1)],
        };
        let fresh = stats_value(
            "/repo/.glyphtrail/ladybug",
            &s,
            &Staleness::Fresh,
            Some(0),
            Some((5, 2)),
        );
        check!(fresh["files"] == json!(3));
        check!(fresh["links"]["configured"] == json!(5));
        check!(fresh["links"]["unresolved"] == json!(2));
        check!(fresh["nodes"] == json!(42));
        check!(fresh["edges"] == json!(100));
        check!(fresh["index"] == json!("/repo/.glyphtrail/ladybug"));
        check!(fresh["languages"]["rust"] == json!(2));
        check!(fresh["languages"]["python"] == json!(1));
        check!(fresh["freshness"] == json!("fresh"));
        check!(fresh["stale"] == json!(false));
        check!(fresh["stale_reason"] == json!(null));
        // An older installed skill is reported outdated; not-onboarded is null.
        check!(fresh["skill_version"] == json!(0));
        check!(fresh["bundled_skill_version"] == json!(setup::SKILL_VERSION));
        check!(fresh["skill_outdated"] == json!(true));

        let stale = stats_value(
            "/x",
            &s,
            &Staleness::Stale("repo is on a new commit".into()),
            None,
            None,
        );
        check!(stale["freshness"] == json!("stale"));
        check!(stale["stale"] == json!(true));
        check!(stale["stale_reason"] == json!("repo is on a new commit"));
        check!(stale["skill_version"] == json!(null));
        check!(stale["skill_outdated"] == json!(false));
        check!(stale["links"] == json!(null)); // no links declared
    }
}
