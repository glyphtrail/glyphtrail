use std::path::Path;

use anyhow::Result;
use glyphtrail_analyze::Staleness;
use glyphtrail_core::config::RepoPaths;
use glyphtrail_store::Stats;
use serde_json::{Value, json};

use crate::commands::backend;
use crate::commands::query::{Emit, print_value};

pub fn run(repo: &Path, emit: Emit) -> Result<()> {
    let paths = RepoPaths::new(repo);
    let store = backend::open_existing(&paths)?;
    let s = store.stats()?;
    let location = backend::location(&paths).display().to_string();
    // Whether the index still reflects the working tree (#313). Reported inline
    // rather than as the stderr note other read commands use.
    let staleness = glyphtrail_analyze::index_staleness(repo, store.as_ref());
    match emit {
        Emit::Text => print_text(&location, &s, &staleness),
        _ => print_value(&stats_value(&location, &s, &staleness), emit)?,
    }
    Ok(())
}

fn print_text(location: &str, s: &Stats, staleness: &Staleness) {
    println!("index:  {location}");
    println!("files:  {}", s.files);
    println!("nodes:  {}", s.nodes);
    println!("edges:  {}", s.edges);
    if !s.languages.is_empty() {
        println!("langs:  {}", format_languages(&s.languages));
    }
    match staleness {
        Staleness::Fresh => println!("status: up to date"),
        Staleness::Stale(why) => {
            println!("status: STALE — {why}; run `glyphtrail analyze` to refresh")
        }
        // Indeterminate (dirty/non-git index): say nothing rather than guess.
        Staleness::Unknown => {}
    }
}

/// Index statistics as a structured value for `--json` / `--yaml` (#109).
fn stats_value(location: &str, s: &Stats, staleness: &Staleness) -> Value {
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
        let fresh = stats_value("/repo/.glyphtrail/ladybug", &s, &Staleness::Fresh);
        check!(fresh["files"] == json!(3));
        check!(fresh["nodes"] == json!(42));
        check!(fresh["edges"] == json!(100));
        check!(fresh["index"] == json!("/repo/.glyphtrail/ladybug"));
        check!(fresh["languages"]["rust"] == json!(2));
        check!(fresh["languages"]["python"] == json!(1));
        check!(fresh["freshness"] == json!("fresh"));
        check!(fresh["stale"] == json!(false));
        check!(fresh["stale_reason"] == json!(null));

        let stale = stats_value(
            "/x",
            &s,
            &Staleness::Stale("repo is on a new commit".into()),
        );
        check!(stale["freshness"] == json!("stale"));
        check!(stale["stale"] == json!(true));
        check!(stale["stale_reason"] == json!("repo is on a new commit"));
    }
}
