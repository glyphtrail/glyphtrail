//! Atlas (#329/#330): the opt-in, local-only global archaeology index. This
//! module is the lifecycle surface — create the store, report its state and the
//! active limits, print its path. Ingestion (#331) and queries (#333) build on
//! it. Atlas writes only under `~/.glyphtrail/atlas/`; nothing here touches the
//! network or exports.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use clap::Subcommand;
use glyphtrail_core::{AtlasConfig, default_atlas_path};
use glyphtrail_store::{GraphStore, LadybugStore};

#[derive(Subcommand)]
pub enum AtlasCmd {
    /// Create the atlas store (opt-in). Idempotent.
    Init,
    /// Report whether atlas is enabled, its path, counts, and the active limits.
    Status,
    /// Print the atlas store's LadybugDB directory.
    Path,
}

/// `~/.glyphtrail/atlas/`, or an error when no home directory is set.
fn atlas_dir() -> Result<PathBuf> {
    default_atlas_path()
        .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))
}

/// The LadybugDB store directory inside the atlas dir.
fn ladybug_dir(atlas: &Path) -> PathBuf {
    atlas.join("ladybug")
}

pub fn run(cmd: AtlasCmd) -> Result<()> {
    let dir = atlas_dir()?;
    match cmd {
        AtlasCmd::Init => {
            std::fs::create_dir_all(&dir)?;
            // Open once to stamp the schema (creates the ladybug dir + tables).
            LadybugStore::open(&ladybug_dir(&dir))?;
            println!("atlas initialized at {}", dir.display());
        }
        AtlasCmd::Path => println!("{}", ladybug_dir(&dir).display()),
        AtlasCmd::Status => status(&dir)?,
    }
    Ok(())
}

/// Report state + the active limits. Establishes the convention every later
/// atlas command follows: always echo the gates so nothing is excluded silently.
fn status(dir: &Path) -> Result<()> {
    let lb = ladybug_dir(dir);
    if !lb.exists() {
        println!("atlas:  disabled (run `glyphtrail atlas init` to enable)");
        println!("would live at: {}", dir.display());
        return Ok(());
    }
    let store = LadybugStore::open(&lb)?;
    let stats = store.stats()?;
    let commits = store.commits_in_range(None, None)?.len();
    let cfg = AtlasConfig::load(dir)?;
    let window = match (&cfg.window.earliest, &cfg.window.latest) {
        (None, None) => "none".to_string(),
        (earliest, latest) => format!(
            "{}..{}",
            earliest.as_deref().unwrap_or(""),
            latest.as_deref().unwrap_or("")
        ),
    };

    println!("atlas:   enabled");
    println!("path:    {}", lb.display());
    println!("nodes:   {}", stats.nodes);
    println!("edges:   {}", stats.edges);
    println!("commits: {commits}");
    println!("window:  {window}");
    Ok(())
}
