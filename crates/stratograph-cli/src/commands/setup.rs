//! `stratograph setup` — onboard coding agents onto stratograph in a repo (#245).
//!
//! Writes a `.claude/skills/stratograph/SKILL.md` skill and a managed section in
//! `CLAUDE.md` and `AGENTS.md` that point an agent at stratograph's MCP/CLI for
//! code understanding and blast-radius analysis (instead of `ls`/`grep` loops),
//! and adds `.stratograph/` to `.gitignore`.
//!
//! Idempotent and static: the agent-file section lives between HTML-comment
//! markers and is replaced in place on re-run, and it carries no stats — so it
//! doesn't dirty the files on every commit (a pitfall of similar tools). Run by
//! the user explicitly; `analyze` never modifies repo files, it only hints.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const BEGIN: &str = "<!-- stratograph:begin (managed section — edits are overwritten) -->";
const END: &str = "<!-- stratograph:end -->";

/// The skill written to `.claude/skills/stratograph/SKILL.md`.
const SKILL_MD: &str = r#"---
name: stratograph
description: >-
  Use when understanding or changing code in this repo: locating where a symbol
  is defined, what calls it, how a request flows across the API boundary, or the
  blast radius of a change (which symbols, tests, endpoints — and which other
  indexed repos — a change affects). Prefer this over ls/grep sweeps.
---

# Stratograph: query the code graph instead of grepping

This repository is indexed by [stratograph](https://github.com/sunsided/stratograph)
as a semantic graph (definitions, calls, imports, inheritance, API endpoints and
their clients). Query that graph for code understanding and impact analysis —
it's faster and more precise than scanning files, and it crosses the web and
package boundaries.

## How to ask

Stratograph exposes an MCP server (`stratograph mcp`, also `POST /mcp` under
`stratograph serve`) and a CLI. Use whichever is wired up.

**Locate / explore**
- `search <query>` — full-text over symbol names and docs.
- `definition <name>` / `callers <name>` / `callees <name>` / `neighbors <name>`.
- `endpoints` / `clients` / `serves <path>` / `who_calls <path>` / `api_impact <path>` — API surface and cross-boundary consumers.

**Blast radius (do this before changing a symbol)**
- `impact <name>` — classified, confidence-aware dependents: tests to run, API
  surface touched, internal dependents. Seed from a symbol, a `--file`, or a git
  change set (`--since`, `--staged`, `--diff`).
- `impact <name> --downstream` (or `--group <name>`) — extends the blast radius
  into *other* indexed repos that depend on this one, reporting which break and
  where.
- `list_repos` — the repos stratograph has indexed (with health + forge ids).

CLI equivalents: `stratograph query <verb> <name>`, `stratograph impact …`.

## Rule of thumb

Reaching for `grep`/`ls` to trace callers, find a definition, or guess what a
change affects? Ask stratograph instead — and run `impact` before editing a
widely-used symbol so you know the tests and consumers up front.
"#;

/// The body of the managed agent-file section (wrapped in the markers).
const AGENT_SECTION_BODY: &str = r#"# Code graph (stratograph)

This repo is indexed by [stratograph](https://github.com/sunsided/stratograph).
For code understanding and change-impact analysis, query the graph via the
stratograph MCP server (`stratograph mcp`) or CLI rather than `ls`/`grep`:

- find code: `search`, `definition`, `callers`, `callees`, `neighbors`
- API flow: `endpoints`, `clients`, `who_calls`, `api_impact`
- **blast radius before a change**: `impact <symbol>` (add `--downstream` to
  reach other indexed repos that depend on this one)

See `.claude/skills/stratograph/SKILL.md` for details."#;

/// Onboard the repo at `root`: write the skill, upsert the managed section in
/// `CLAUDE.md`/`AGENTS.md`, and ignore `.stratograph/`. Idempotent.
pub fn run(root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", root.display()))?;

    write_skill(&root)?;
    println!("wrote .claude/skills/stratograph/SKILL.md");

    for name in ["CLAUDE.md", "AGENTS.md"] {
        let created = upsert_section(&root.join(name))?;
        println!("{} {name}", if created { "created" } else { "updated" });
    }

    if ensure_gitignore(&root)? {
        println!("added .stratograph/ to .gitignore");
    }
    Ok(())
}

fn write_skill(root: &Path) -> Result<()> {
    let dir = root.join(".claude").join("skills").join("stratograph");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(dir.join("SKILL.md"), SKILL_MD).context("writing SKILL.md")?;
    Ok(())
}

/// Insert or replace the managed section in `path`, returning whether the file
/// was created. The section is delimited by [`BEGIN`]/[`END`]; on re-run the
/// span between them is replaced, so the rest of the file is untouched.
fn upsert_section(path: &Path) -> Result<bool> {
    let block = format!("{BEGIN}\n{AGENT_SECTION_BODY}\n{END}\n");
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

/// Add `.stratograph/` to `.gitignore` if not already ignored. Returns whether
/// a line was added.
fn ensure_gitignore(root: &Path) -> Result<bool> {
    let path = root.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| matches!(l.trim(), ".stratograph" | ".stratograph/"))
    {
        return Ok(false);
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(".stratograph/\n");
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
        let dir = std::env::temp_dir().join(format!("stratograph-setup-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn setup_writes_files_and_is_idempotent() {
        let dir = temp_dir("idem");
        std::fs::write(dir.join("CLAUDE.md"), "# My project\n\nNotes.\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();

        run(&dir).unwrap();
        let claude1 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(claude1.starts_with("# My project")); // preserved existing content
        check!(claude1.contains(BEGIN) && claude1.contains("Code graph (stratograph)"));
        check!(
            std::fs::read_to_string(dir.join("AGENTS.md"))
                .unwrap()
                .contains(BEGIN)
        );
        check!(
            std::fs::read_to_string(dir.join(".claude/skills/stratograph/SKILL.md"))
                .unwrap()
                .contains("name: stratograph")
        );
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi.contains("target/") && gi.contains(".stratograph/"));

        // Re-run: no duplication.
        run(&dir).unwrap();
        let claude2 = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        check!(claude2 == claude1);
        check!(claude2.matches(BEGIN).count() == 1);
        let gi2 = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        check!(gi2.matches(".stratograph/").count() == 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
