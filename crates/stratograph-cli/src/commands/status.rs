use std::path::Path;

use anyhow::Result;
use stratograph_core::config::RepoPaths;

use crate::commands::backend;

pub fn run(repo: &Path) -> Result<()> {
    let paths = RepoPaths::new(repo);
    let store = backend::open_existing(&paths)?;
    let s = store.stats()?;
    println!("index:  {}", backend::location(&paths).display());
    println!("files:  {}", s.files);
    println!("nodes:  {}", s.nodes);
    println!("edges:  {}", s.edges);
    if !s.languages.is_empty() {
        println!("langs:  {}", format_languages(&s.languages));
    }
    Ok(())
}

/// Render per-language file counts as `rust 12, python 3`, descending.
pub fn format_languages(languages: &[(String, usize)]) -> String {
    languages
        .iter()
        .map(|(lang, n)| format!("{lang} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}
