//! `glyphtrail setup` — onboard coding agents onto glyphtrail in a repo (#245).
//!
//! Writes a `.claude/skills/glyphtrail/SKILL.md` skill and a managed section in
//! `CLAUDE.md` and `AGENTS.md` that point an agent at glyphtrail's MCP/CLI for
//! code understanding and blast-radius analysis (instead of `ls`/`grep` loops),
//! and adds `.glyphtrail/` to `.gitignore`.
//!
//! Idempotent and static: the agent-file section lives between HTML-comment
//! markers and is replaced in place on re-run, and it carries no stats — so it
//! doesn't dirty the files on every commit (a pitfall of similar tools). Run by
//! the user explicitly; `analyze` never modifies repo files, it only hints.
//!
//! Safe outside a repo (#245): writing agent files into a random directory is
//! usually a mistake, so a target that isn't inside a git repository errors —
//! unless `--force` (write here anyway) or `--home` (write the global agent
//! files to the user's home directory) is given.
//!
//! The skill and managed-section content are *not* inlined here: they are the
//! repo's own dogfooded onboarding files (`.claude/skills/glyphtrail/SKILL.md`
//! and the managed section of `CLAUDE.md`), copied into `assets/` and embedded
//! below via `include_str!`. `build.rs` fails the build if a copy drifts from
//! its source; `task assets:sync` regenerates the copies.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

const BEGIN: &str = "<!-- glyphtrail:begin (managed section — edits are overwritten) -->";
const END: &str = "<!-- glyphtrail:end -->";

/// The skill written to `.claude/skills/glyphtrail/SKILL.md`. Bundled copy of
/// the repo's own skill of the same name (see `build.rs`, `task assets:sync`).
const SKILL_MD: &str = include_str!("../../assets/SKILL.md");

/// The body of the managed agent-file section (wrapped in the markers). Bundled
/// copy of the managed section of the repo's own `CLAUDE.md`; carries a trailing
/// newline, so the block is `{BEGIN}\n{body}{END}\n`.
const AGENT_SECTION_BODY: &str = include_str!("../../assets/agent-section.md");

/// Onboard agents: write the skill, upsert the managed section in
/// `CLAUDE.md`/`AGENTS.md`, and (inside a repo) ignore `.glyphtrail/`.
/// Idempotent.
///
/// `home` targets the user's home directory (global agent config) instead of
/// `path`. Otherwise, when `path` is not inside a git repository, this errors
/// unless `force` is set — so the files don't land in a random directory by
/// mistake.
pub fn run(path: &Path, force: bool, home: bool) -> Result<()> {
    let target = if home {
        home_dir()
            .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))?
    } else {
        path.canonicalize()
            .with_context(|| format!("cannot resolve path {}", path.display()))?
    };

    let in_repo = in_git_repo(&target);
    if !home && !force && !in_repo {
        bail!(
            "{} is not inside a git repository.\n  \
             Run setup inside a repo, or pass --force to write here anyway, \
             or --home to write global agent files to your home directory.",
            target.display()
        );
    }

    write_skill(&target)?;
    println!(
        "wrote {}",
        target.join(".claude/skills/glyphtrail/SKILL.md").display()
    );

    for name in ["CLAUDE.md", "AGENTS.md"] {
        let created = upsert_section(&target.join(name))?;
        println!(
            "{} {}",
            if created { "created" } else { "updated" },
            target.join(name).display()
        );
    }

    // `.gitignore` only makes sense inside a repository.
    if in_repo && ensure_gitignore(&target)? {
        println!("added .glyphtrail/ to .gitignore");
    }
    Ok(())
}

/// Whether `path` is inside a git repository (it or an ancestor has `.git`).
fn in_git_repo(path: &Path) -> bool {
    path.ancestors().any(|p| p.join(".git").exists())
}

/// The user's home directory from `HOME` (or `USERPROFILE` on Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn write_skill(root: &Path) -> Result<()> {
    let dir = root.join(".claude").join("skills").join("glyphtrail");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(dir.join("SKILL.md"), SKILL_MD).context("writing SKILL.md")?;
    Ok(())
}

/// Insert or replace the managed section in `path`, returning whether the file
/// was created. The section is delimited by [`BEGIN`]/[`END`]; on re-run the
/// span between them is replaced, so the rest of the file is untouched.
fn upsert_section(path: &Path) -> Result<bool> {
    // `AGENT_SECTION_BODY` already ends with a newline, so the block reads
    // `{BEGIN}\n{body}{END}\n` — never blindly overwrite the file; only this
    // span between the markers is owned by glyphtrail.
    let block = format!("{BEGIN}\n{AGENT_SECTION_BODY}{END}\n");
    let existing = fs::read_to_string(path).unwrap_or_default();
    let created = existing.is_empty();

    let updated = match (existing.find(BEGIN), existing.find(END)) {
        (Some(b), Some(e)) if e > b => {
            let end = e + END.len();
            // Drop a trailing newline inside the old span so we don't accumulate blanks.
            let tail = existing[end..]
                .strip_prefix('\n')
                .unwrap_or(&existing[end..]);
            format!("{}{}{}", &existing[..b], block, tail)
        }
        _ => {
            let mut s = existing;
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&block);
            s
        }
    };
    fs::write(path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(created)
}

/// Add `.glyphtrail/` to `.gitignore` if not already ignored. Returns whether
/// a line was added.
fn ensure_gitignore(root: &Path) -> Result<bool> {
    let path = root.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| matches!(l.trim(), ".glyphtrail" | ".glyphtrail/"))
    {
        return Ok(false);
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(".glyphtrail/\n");
    fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("glyphtrail-setup-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The managed-section body (with its trailing newline) between the markers.
    fn section(s: &str) -> &str {
        let b = s.find(BEGIN).unwrap();
        let after = &s[b + BEGIN.len()..];
        let start = b + BEGIN.len() + after.find('\n').unwrap() + 1;
        let e = s[start..].find(END).unwrap() + start;
        &s[start..e]
    }

    /// Repo root, two levels up from this crate's manifest dir.
    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn setup_writes_files_and_is_idempotent() {
        let dir = temp_dir("idem");
        std::fs::create_dir_all(dir.join(".git")).unwrap(); // make it a repo
        std::fs::write(dir.join("CLAUDE.md"), "# My project\n\nNotes.\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();

        run(&dir, false, false).unwrap();
        let claude1 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(claude1.starts_with("# My project")); // preserved existing content
        check!(claude1.contains(BEGIN) && claude1.contains("Code graph (glyphtrail)"));
        check!(
            std::fs::read_to_string(dir.join("AGENTS.md"))
                .unwrap()
                .contains(BEGIN)
        );
        check!(
            std::fs::read_to_string(dir.join(".claude/skills/glyphtrail/SKILL.md"))
                .unwrap()
                .contains("name: glyphtrail")
        );
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi.contains("target/") && gi.contains(".glyphtrail/"));

        // Re-run: no duplication.
        run(&dir, false, false).unwrap();
        let claude2 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(claude2 == claude1);
        check!(claude2.matches(BEGIN).count() == 1);
        let gi2 = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi2.matches(".glyphtrail/").count() == 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    // #245: outside a git repo, setup errors by default but `--force` writes
    // anyway (and skips the repo-only .gitignore step).
    #[test]
    fn setup_outside_repo_errors_unless_forced() {
        let dir = temp_dir("no-repo"); // no .git
        check!(run(&dir, false, false).is_err());
        check!(!dir.join("CLAUDE.md").exists()); // nothing written on error

        run(&dir, true, false).unwrap(); // --force
        check!(dir.join(".claude/skills/glyphtrail/SKILL.md").exists());
        check!(dir.join("CLAUDE.md").exists());
        check!(!dir.join(".gitignore").exists()); // not a repo -> no gitignore

        std::fs::remove_dir_all(&dir).ok();
    }

    // Dogfood: what `setup` installs must be byte-for-byte the repo's own
    // committed onboarding files (the source the bundled assets are copied
    // from). Guards against the embedded copy drifting from what we ship.
    #[test]
    fn setup_reproduces_committed_repo_files() {
        let root = repo_root();
        let dir = temp_dir("dogfood");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        run(&dir, false, false).unwrap();

        let got_skill =
            std::fs::read_to_string(dir.join(".claude/skills/glyphtrail/SKILL.md")).unwrap();
        let want_skill =
            std::fs::read_to_string(root.join(".claude/skills/glyphtrail/SKILL.md")).unwrap();
        check!(got_skill == want_skill);

        let got = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        let want = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        check!(section(&got) == section(&want));

        std::fs::remove_dir_all(&dir).ok();
    }
}
