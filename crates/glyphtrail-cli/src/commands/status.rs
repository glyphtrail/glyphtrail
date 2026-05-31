use std::path::Path;

use anyhow::Result;
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
    match emit {
        Emit::Text => print_text(&location, &s),
        _ => print_value(&stats_value(&location, &s), emit)?,
    }
    Ok(())
}

fn print_text(location: &str, s: &Stats) {
    println!("index:  {location}");
    println!("files:  {}", s.files);
    println!("nodes:  {}", s.nodes);
    println!("edges:  {}", s.edges);
    if !s.languages.is_empty() {
        println!("langs:  {}", format_languages(&s.languages));
    }
}

/// Index statistics as a structured value for `--json` / `--yaml` (#109).
fn stats_value(location: &str, s: &Stats) -> Value {
    let languages: serde_json::Map<String, Value> = s
        .languages
        .iter()
        .map(|(lang, n)| (lang.clone(), json!(n)))
        .collect();
    json!({
        "index": location,
        "files": s.files,
        "nodes": s.nodes,
        "edges": s.edges,
        "languages": languages,
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
        let v = stats_value("/repo/.glyphtrail/ladybug", &s);
        check!(v["files"] == json!(3));
        check!(v["nodes"] == json!(42));
        check!(v["edges"] == json!(100));
        check!(v["index"] == json!("/repo/.glyphtrail/ladybug"));
        check!(v["languages"]["rust"] == json!(2));
        check!(v["languages"]["python"] == json!(1));
    }
}
