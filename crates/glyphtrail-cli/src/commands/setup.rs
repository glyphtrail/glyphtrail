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
//! The destination is explicit (#390): `--local` writes into the repo, `--user`
//! into the home directory, and `setup` errors without one (a repo-local install
//! lands in commits, so it never guesses). Safe outside a repo (#245): `--local`
//! in a non-repository directory errors unless `--force` is given.
//!
//! The skill and managed-section content are *not* inlined here: they are the
//! repo's own dogfooded onboarding files (`.claude/skills/glyphtrail/SKILL.md`
//! and the managed section of `CLAUDE.md`), copied into `assets/` and embedded
//! below via `include_str!`. `build.rs` fails the build if a copy drifts from
//! its source; `task assets:sync` regenerates the copies.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Version of the bundled skill + managed section. Bump when the content in
/// `assets/` changes (and the `glyphtrail-version` in the skill frontmatter).
/// An install stamped with a lower version is reported stale — a hint only;
/// `analyze`/`status` never rewrite the files, only `setup` does.
pub const SKILL_VERSION: u32 = 1;

/// Version-independent prefix of the begin marker, so an existing section is
/// detected (and replaced) regardless of the `v=` it was written with.
const BEGIN_PREFIX: &str = "<!-- glyphtrail:begin";
const END: &str = "<!-- glyphtrail:end -->";

/// The begin marker for the current [`SKILL_VERSION`].
fn begin_marker() -> String {
    format!("<!-- glyphtrail:begin v={SKILL_VERSION} (managed section — edits are overwritten) -->")
}

/// The skill written to `.claude/skills/glyphtrail/SKILL.md`. Bundled copy of
/// the repo's own skill of the same name (see `build.rs`, `task assets:sync`).
const SKILL_MD: &str = include_str!("../../assets/SKILL.md");

/// The body of the managed agent-file section (wrapped in the markers). Bundled
/// copy of the managed section of the repo's own `CLAUDE.md`; carries a trailing
/// newline, so the block is `{BEGIN}\n{body}{END}\n`.
const AGENT_SECTION_BODY: &str = include_str!("../../assets/agent-section.md");

/// Onboard agents by writing the skill and the managed `CLAUDE.md`/`AGENTS.md`
/// section. The destination is chosen explicitly (#390) — a repo-local install
/// ends up in commits, so `setup` never guesses: pass `local` (this repo),
/// `user` (your home directory), or both. With neither, it errors.
///
/// `force` lets `--local` write outside a git repository. `gitignore` keeps the
/// local install out of VCS (skill written but gitignored, no `CLAUDE.md` patch,
/// any prior section stripped); it applies to the local target only.
pub fn run(
    path: &Path,
    local: bool,
    user: bool,
    force: bool,
    gitignore: bool,
    mcp: bool,
) -> Result<()> {
    if !local && !user {
        bail!(
            "choose where to write agent files: --local (this repository) and/or \
             --user (your home directory). A repo-local install lands in commits, \
             so setup won't guess."
        );
    }
    if gitignore && !local {
        bail!("--gitignore applies to a repository's files; pass it together with --local");
    }

    if local {
        let target = path
            .canonicalize()
            .with_context(|| format!("cannot resolve path {}", path.display()))?;
        let in_repo = in_git_repo(&target);
        if !force && !in_repo {
            bail!(
                "{} is not inside a git repository.\n  \
                 Run --local inside a repo, or pass --force to write here anyway.",
                target.display()
            );
        }
        apply_to(&target, in_repo, gitignore)?;
        if mcp {
            // Register the project-scoped MCP server (#403) in the repo's
            // `.mcp.json` — the cross-client project MCP config.
            write_mcp_json(&target)?;
        }
    }

    if user {
        let target = home_dir()
            .ok_or_else(|| anyhow!("cannot locate home directory (set HOME or USERPROFILE)"))?;
        // Home is not a repository, so there is no `.gitignore` to touch and
        // `--gitignore` (local-only) does not apply here.
        apply_to(&target, false, false)?;
        if mcp {
            print_user_mcp_guidance();
        }
    }
    Ok(())
}

/// Register glyphtrail as the project-scoped MCP server by merging a `glyphtrail`
/// entry into `<root>/.mcp.json`, preserving any other servers (#403).
fn write_mcp_json(root: &Path) -> Result<()> {
    use serde_json::{Value, json};
    let path = root.join(".mcp.json");
    let mut doc: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; not overwriting", path.display()))?
    } else {
        json!({})
    };
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("\"mcpServers\" in {} is not an object", path.display()))?;
    let existed = servers.contains_key("glyphtrail");
    servers.insert(
        "glyphtrail".to_string(),
        json!({ "command": "glyphtrail", "args": ["mcp", "--repo", "."] }),
    );
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&doc)?))
        .with_context(|| format!("writing {}", path.display()))?;
    println!(
        "{} glyphtrail MCP server in {}",
        if existed { "updated" } else { "registered" },
        path.display()
    );
    Ok(())
}

/// Print the snippet for adding glyphtrail to a user-wide MCP client config,
/// since the location varies by client (#403).
fn print_user_mcp_guidance() {
    println!(
        "\nTo register glyphtrail user-wide, add this server to your MCP client's \
         config (e.g. Claude Code `~/.claude.json`, or Claude Desktop's config):\n\
         \n  \"glyphtrail\": {{ \"command\": \"glyphtrail\", \"args\": [\"mcp\"] }}\n\
         \n(No `--repo`, so every tool call names the repo by registered name or path.)"
    );
}

/// Install (or, with `gitignore`, un-install) the agent files under `target`.
/// `in_repo` enables the `.gitignore` edits, which only make sense inside a repo.
fn apply_to(target: &Path, in_repo: bool, gitignore: bool) -> Result<()> {
    // Capture the previously-installed version before we overwrite anything, so
    // we can report a version transition below.
    let prior = installed_version(target);

    write_skill(target)?;
    println!(
        "wrote {}",
        target.join(".claude/skills/glyphtrail/SKILL.md").display()
    );

    for name in ["CLAUDE.md", "AGENTS.md"] {
        let file = target.join(name);
        if gitignore {
            // Local-only: strip a previously-installed section, leave the rest.
            if remove_section(&file)? {
                println!("removed glyphtrail section from {}", file.display());
            }
        } else {
            let created = upsert_section(&file)?;
            println!(
                "{} {}",
                if created { "created" } else { "updated" },
                file.display()
            );
        }
    }

    // `.gitignore` only makes sense inside a repository.
    if in_repo {
        if add_ignore(target, ".glyphtrail/")? {
            println!("added .glyphtrail/ to .gitignore");
        }
        if gitignore && add_ignore(target, ".claude/skills/glyphtrail/")? {
            println!("added .claude/skills/glyphtrail/ to .gitignore");
        }
    }

    // A version transition only makes sense when we (re)wrote the section.
    if !gitignore
        && let Some(v) = prior
        && v < SKILL_VERSION
    {
        println!("updated glyphtrail agent files (v{v} -> v{SKILL_VERSION})");
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
/// was created. The section is delimited by the `glyphtrail:begin`/
/// `glyphtrail:end` markers ([`BEGIN_PREFIX`]/[`END`]); on re-run the span
/// between them is replaced, so the rest of the file is untouched.
fn upsert_section(path: &Path) -> Result<bool> {
    // `AGENT_SECTION_BODY` already ends with a newline, so the block reads
    // `{begin}\n{body}{END}\n` — never blindly overwrite the file; only this
    // span between the markers is owned by glyphtrail.
    let block = format!("{}\n{AGENT_SECTION_BODY}{END}\n", begin_marker());
    let existing = fs::read_to_string(path).unwrap_or_default();
    let created = existing.is_empty();

    // Find the end marker *after* the begin marker — a stray `glyphtrail:end`
    // earlier in the file must not be mistaken for our section's terminator.
    let begin = existing.find(BEGIN_PREFIX);
    let end_start = begin.and_then(|b| existing[b..].find(END).map(|e| b + e));
    let updated = match (begin, end_start) {
        (Some(b), Some(e)) => {
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

/// Remove the managed section (markers, body, and the blank line `upsert_section`
/// inserted before it) from `path`, preserving everything else. Returns whether
/// a section was removed; a no-op (returning `false`) when the file is absent or
/// has no section. The inverse of [`upsert_section`].
fn remove_section(path: &Path) -> Result<bool> {
    let Ok(existing) = fs::read_to_string(path) else {
        return Ok(false);
    };
    // Match `upsert_section`: locate the end marker after the begin marker.
    let Some(b) = existing.find(BEGIN_PREFIX) else {
        return Ok(false);
    };
    let Some(rel) = existing[b..].find(END) else {
        return Ok(false);
    };
    let end = b + rel + END.len();

    // The block ends with a newline; drop it so it doesn't linger as a blank
    // line. Trim the blank-line separator we added before the block, too.
    let after = existing[end..]
        .strip_prefix('\n')
        .unwrap_or(&existing[end..]);
    let before = existing[..b].trim_end_matches('\n');

    let mut out = String::from(before);
    if !before.is_empty() && !after.is_empty() {
        out.push('\n'); // single separator between preserved halves
    }
    out.push_str(after);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    fs::write(path, &out).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Add `line` to `.gitignore` if not already ignored (matching with or without a
/// trailing slash). Returns whether a line was added.
fn add_ignore(root: &Path, line: &str) -> Result<bool> {
    let path = root.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let bare = line.trim_end_matches('/');
    if existing.lines().any(|l| {
        let t = l.trim();
        t == line || t == bare
    }) {
        return Ok(false);
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(line);
    s.push('\n');
    fs::write(&path, s).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// The glyphtrail version installed in `repo`, if onboarded. Prefers the skill's
/// frontmatter (written in every mode); falls back to the `v=` in the
/// `CLAUDE.md` managed-section marker. An onboarded repo without a version
/// stamp (a pre-versioning install) counts as 0.
pub fn installed_version(repo: &Path) -> Option<u32> {
    if let Ok(skill) = fs::read_to_string(repo.join(".claude/skills/glyphtrail/SKILL.md")) {
        return Some(frontmatter_version(&skill));
    }
    let claude = fs::read_to_string(repo.join("CLAUDE.md")).ok()?;
    let b = claude.find(BEGIN_PREFIX)?;
    let line_end = claude[b..].find('\n').map_or(claude.len(), |n| b + n);
    Some(marker_version(&claude[b..line_end]))
}

/// A stale-skill advisory for `repo`, if the installed version is older than the
/// bundled [`SKILL_VERSION`]. `None` when current, newer, or not onboarded.
pub fn staleness_hint(repo: &Path) -> Option<String> {
    let installed = installed_version(repo)?;
    (installed < SKILL_VERSION).then(|| {
        format!(
            "note: glyphtrail agent files are out of date \
             (installed v{installed}, bundled v{SKILL_VERSION}) — \
             run `glyphtrail setup --local` (or --user) to update"
        )
    })
}

/// Parse `glyphtrail-version: N` from skill frontmatter (0 if absent).
fn frontmatter_version(skill: &str) -> u32 {
    skill
        .lines()
        .find_map(|l| l.trim().strip_prefix("glyphtrail-version:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Parse the `v=N` stamped in a begin-marker line (0 if absent).
fn marker_version(line: &str) -> u32 {
    line.find("v=")
        .and_then(|i| {
            let digits: String = line[i + 2..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
        .unwrap_or(0)
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
        let b = s.find(BEGIN_PREFIX).unwrap();
        let start = b + s[b..].find('\n').unwrap() + 1;
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

    // #403: `setup --mcp` registers the project MCP server in `.mcp.json`,
    // preserving any other servers already there.
    #[test]
    fn write_mcp_json_merges_and_preserves_other_servers() {
        let dir = temp_dir("mcp-json");
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();
        write_mcp_json(&dir).unwrap();
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
        check!(doc["mcpServers"]["glyphtrail"]["command"] == serde_json::json!("glyphtrail"));
        check!(doc["mcpServers"]["glyphtrail"]["args"][0] == serde_json::json!("mcp"));
        check!(doc["mcpServers"]["other"]["command"] == serde_json::json!("x")); // preserved
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setup_writes_files_and_is_idempotent() {
        let dir = temp_dir("idem");
        std::fs::create_dir_all(dir.join(".git")).unwrap(); // make it a repo
        std::fs::write(dir.join("CLAUDE.md"), "# My project\n\nNotes.\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();

        run(&dir, true, false, false, false, false).unwrap();
        let claude1 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(claude1.starts_with("# My project")); // preserved existing content
        check!(claude1.contains(BEGIN_PREFIX) && claude1.contains("Code graph (glyphtrail)"));
        check!(
            std::fs::read_to_string(dir.join("AGENTS.md"))
                .unwrap()
                .contains(BEGIN_PREFIX)
        );
        check!(
            std::fs::read_to_string(dir.join(".claude/skills/glyphtrail/SKILL.md"))
                .unwrap()
                .contains("name: glyphtrail")
        );
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi.contains("target/") && gi.contains(".glyphtrail/"));

        // Re-run: no duplication.
        run(&dir, true, false, false, false, false).unwrap();
        let claude2 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(claude2 == claude1);
        check!(claude2.matches(BEGIN_PREFIX).count() == 1);
        let gi2 = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi2.matches(".glyphtrail/").count() == 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    // #245: outside a git repo, setup errors by default but `--force` writes
    // anyway (and skips the repo-only .gitignore step).
    #[test]
    fn setup_outside_repo_errors_unless_forced() {
        let dir = temp_dir("no-repo"); // no .git
        check!(run(&dir, true, false, false, false, false).is_err());
        check!(!dir.join("CLAUDE.md").exists()); // nothing written on error

        run(&dir, true, false, true, false, false).unwrap(); // --force
        check!(dir.join(".claude/skills/glyphtrail/SKILL.md").exists());
        check!(dir.join("CLAUDE.md").exists());
        check!(!dir.join(".gitignore").exists()); // not a repo -> no gitignore

        std::fs::remove_dir_all(&dir).ok();
    }

    // A stray end-marker *before* the section (e.g. quoted in prose) must not be
    // mistaken for our terminator — otherwise a re-run appends a duplicate (#392).
    #[test]
    fn upsert_ignores_end_marker_before_begin() {
        let dir = temp_dir("stray-end");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join("CLAUDE.md"),
            "# Notes\n\nExample delimiter: <!-- glyphtrail:end -->\n",
        )
        .unwrap();

        run(&dir, true, false, false, false, false).unwrap();
        let c1 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(c1.matches(BEGIN_PREFIX).count() == 1);

        // Re-run: the section is found and replaced in place, not duplicated.
        run(&dir, true, false, false, false, false).unwrap();
        let c2 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(c2 == c1);
        check!(c2.matches(BEGIN_PREFIX).count() == 1);

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
        run(&dir, true, false, false, false, false).unwrap();

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

    // A fresh setup is current (no hint); an install stamped older is flagged.
    #[test]
    fn staleness_hint_fires_only_when_older() {
        let dir = temp_dir("stale");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        run(&dir, true, false, false, false, false).unwrap();
        check!(installed_version(&dir) == Some(SKILL_VERSION));
        check!(staleness_hint(&dir).is_none());

        // Simulate an older install by rewriting the skill's version stamp.
        let skill_path = dir.join(".claude/skills/glyphtrail/SKILL.md");
        let skill = std::fs::read_to_string(&skill_path).unwrap();
        let aged = skill.replace(
            &format!("glyphtrail-version: {SKILL_VERSION}"),
            "glyphtrail-version: 0",
        );
        check!(aged != skill); // the stamp was present and changed
        std::fs::write(&skill_path, aged).unwrap();
        check!(installed_version(&dir) == Some(0));
        check!(staleness_hint(&dir).unwrap().contains("out of date"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // --gitignore (#391): keep glyphtrail local-only — write the skill but
    // gitignore it, skip the CLAUDE.md/AGENTS.md patch, and strip a section a
    // prior run added, restoring the surrounding content exactly.
    #[test]
    fn gitignore_mode_is_local_only_and_strips_section() {
        let dir = temp_dir("gitignore");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "# My project\n\nNotes.\n").unwrap();

        // A normal setup first, which appends the managed section.
        run(&dir, true, false, false, false, false).unwrap();
        check!(
            std::fs::read_to_string(dir.join("CLAUDE.md"))
                .unwrap()
                .contains(BEGIN_PREFIX)
        );

        // --gitignore: section stripped (content restored byte-for-byte), skill
        // still present, both glyphtrail paths gitignored.
        run(&dir, true, false, false, true, false).unwrap();
        let claude = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(claude == "# My project\n\nNotes.\n");
        check!(!claude.contains(BEGIN_PREFIX));
        check!(dir.join(".claude/skills/glyphtrail/SKILL.md").exists());
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi.contains(".glyphtrail/"));
        check!(gi.contains(".claude/skills/glyphtrail/"));

        // Idempotent: re-running adds no duplicate ignore lines.
        run(&dir, true, false, false, true, false).unwrap();
        let gi2 = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi2.matches(".claude/skills/glyphtrail/").count() == 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    // #390: a target is mandatory — bare setup (no --local/--user) errors
    // rather than defaulting to a repo-local install that lands in commits.
    #[test]
    fn setup_requires_a_target() {
        let dir = temp_dir("no-target");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        check!(run(&dir, false, false, false, false, false).is_err());
        check!(!dir.join("CLAUDE.md").exists()); // nothing written
        std::fs::remove_dir_all(&dir).ok();
    }

    // --gitignore is local-only; with --user but no --local it's rejected before
    // any path resolution, so the test never touches the real HOME.
    #[test]
    fn gitignore_requires_local() {
        let dir = temp_dir("conflict");
        check!(run(&dir, false, true, false, true, false).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
