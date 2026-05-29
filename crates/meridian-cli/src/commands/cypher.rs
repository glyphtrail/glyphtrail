//! `meridian cypher` — run a raw Cypher query against the LadybugDB backend (#9).
//!
//! An escape hatch for ad-hoc graph queries. Only meaningful with the
//! `ladybug` feature and a repo analyzed with `--backend ladybug`.

use std::path::Path;

use anyhow::Result;

#[cfg(feature = "ladybug")]
pub fn run(repo: &Path, query: &str) -> Result<()> {
    use anyhow::bail;
    use meridian_core::config::RepoPaths;
    use meridian_store::LadybugStore;

    let dir = RepoPaths::new(repo).index_dir.join("ladybug");
    if !dir.exists() {
        bail!(
            "no LadybugDB index at {} — run `meridian analyze --backend ladybug` first",
            dir.display()
        );
    }
    let store = LadybugStore::open(&dir)?;
    print!("{}", store.cypher(query)?);
    Ok(())
}

#[cfg(not(feature = "ladybug"))]
pub fn run(_repo: &Path, _query: &str) -> Result<()> {
    anyhow::bail!("`cypher` requires the LadybugDB backend; rebuild with `--features ladybug`")
}
