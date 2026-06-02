//! Sync guard for the bundled agent assets.
//!
//! `setup` embeds `assets/SKILL.md` and `assets/agent-section.md` (via
//! `include_str!`) into the binary. Those are *copies* of the repo's own
//! source-of-truth onboarding files (`.claude/skills/glyphtrail/SKILL.md` and
//! the managed section of `CLAUDE.md`) — see `task assets:sync`. This script
//! fails the build if a copy drifts from its source, so the bundled content can
//! never silently fall behind what the repo dogfoods.
//!
//! On an isolated `cargo publish` verify build the repo-root sources are absent;
//! there the bundled copies are authoritative and the check is skipped.
//!
//! Keep `BEGIN_PREFIX`/`END` in sync with the same constants in
//! `src/commands/setup.rs`. The begin marker carries a `v=N` version stamp, so
//! match on the version-independent prefix.

use std::path::Path;

const BEGIN_PREFIX: &str = "<!-- glyphtrail:begin";
const END: &str = "<!-- glyphtrail:end -->";

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest = Path::new(&manifest);

    println!("cargo:rerun-if-changed=assets/SKILL.md");
    println!("cargo:rerun-if-changed=assets/agent-section.md");

    // crates/glyphtrail-cli -> crates -> repo root
    let Some(root) = manifest.parent().and_then(Path::parent) else {
        return;
    };
    let root_skill = root.join(".claude/skills/glyphtrail/SKILL.md");
    let root_claude = root.join("CLAUDE.md");
    println!("cargo:rerun-if-changed={}", root_skill.display());
    println!("cargo:rerun-if-changed={}", root_claude.display());

    // Isolated package build (publish): no repo-root sources — bundled copies win.
    if !root_skill.exists() || !root_claude.exists() {
        return;
    }

    if read(&root_skill) != read(&manifest.join("assets/SKILL.md")) {
        fail("SKILL.md");
    }

    let claude = read(&root_claude);
    let section = section_body(&claude).unwrap_or_else(|| {
        bail("could not find the glyphtrail managed section (markers) in CLAUDE.md")
    });
    if section != read(&manifest.join("assets/agent-section.md")) {
        fail("agent-section.md");
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

/// The managed-section body (including its trailing newline) between the markers.
fn section_body(s: &str) -> Option<String> {
    let b = s.find(BEGIN_PREFIX)?;
    let nl = s[b..].find('\n')?; // end of the begin-marker line
    let start = b + nl + 1; // first byte of the body
    let e = s[start..].find(END)? + start; // start of the end marker
    Some(s[start..e].to_string())
}

fn fail(asset: &str) -> ! {
    bail(&format!(
        "bundled agent asset `assets/{asset}` is out of date with its repo-root source — run `task assets:sync`"
    ))
}

fn bail(msg: &str) -> ! {
    panic!("{msg}");
}
