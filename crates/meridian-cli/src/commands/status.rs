use std::path::Path;

use anyhow::{Result, bail};
use meridian_core::config::RepoPaths;
use meridian_store::SqliteStore;

pub fn run(repo: &Path) -> Result<()> {
    let paths = RepoPaths::new(repo);
    if !paths.db_path.exists() {
        bail!(
            "no index found at {} — run `meridian analyze` first",
            paths.db_path.display()
        );
    }
    let store = SqliteStore::open(&paths.db_path)?;
    let s = store.stats()?;
    println!("index:  {}", paths.db_path.display());
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
