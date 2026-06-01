//! Map a git change into impact-analysis seed nodes (#70).
//!
//! A change specification (working tree, staged, or a commit range) is turned
//! into changed files + new-side line ranges by parsing `git diff
//! --unified=0`. Those ranges are intersected with indexed node spans to yield
//! seed node ids for the traversal engine. Git is invoked as a subprocess to
//! keep the dependency surface light. Shared by the CLI `impact` command and the
//! MCP `impact` tool so neither duplicates seeding logic.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use glyphtrail_core::NodeId;

#[cfg(test)]
use crate::LadybugStore;
use crate::graph_store::GraphStore;

/// What change to seed the impact analysis from.
#[derive(Debug, Clone)]
pub enum ChangeSpec {
    /// Unstaged working-tree changes (`git diff`).
    WorkingTree,
    /// Staged changes (`git diff --cached`).
    Staged,
    /// A git revision or range, diffed against the working tree, e.g. `main` or
    /// `main..HEAD` (`git diff --unified=0 <rev>`).
    Since(String),
    /// Explicit repo-relative files; every symbol in each is seeded.
    Files(Vec<String>),
}

/// A changed file and the new-side line ranges that changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// Inclusive new-side line ranges. Empty means "whole file" (explicit
    /// `--files`) or a pure deletion hunk.
    pub ranges: Vec<(usize, usize)>,
    /// The file was deleted by the change (no new-side content).
    pub deleted: bool,
}

/// Seeds resolved from a change set, plus notes for the report.
#[derive(Debug, Clone, Default)]
pub struct SeedSet {
    pub seeds: Vec<NodeId>,
    /// Deleted files: their former dependents are impacted but the symbols are
    /// gone, so they cannot be seeded.
    pub removed_files: Vec<String>,
    /// Changed files with no overlapping indexed symbol.
    pub unresolved_files: Vec<String>,
}

fn run_git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to run git (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Resolve a change specification to the set of changed files + line ranges.
pub fn changed_files(repo: &Path, spec: &ChangeSpec) -> Result<Vec<ChangedFile>> {
    match spec {
        ChangeSpec::Files(paths) => Ok(paths
            .iter()
            .map(|p| ChangedFile {
                path: p.replace('\\', "/"),
                ranges: Vec::new(),
                deleted: false,
            })
            .collect()),
        ChangeSpec::WorkingTree => Ok(parse_diff(&run_git(repo, &["diff", "--unified=0"])?)),
        ChangeSpec::Staged => Ok(parse_diff(&run_git(
            repo,
            &["diff", "--cached", "--unified=0"],
        )?)),
        ChangeSpec::Since(rev) => Ok(parse_diff(&run_git(repo, &["diff", "--unified=0", rev])?)),
    }
}

/// Parse `git diff --unified=0` output into per-file new-side line ranges.
fn parse_diff(text: &str) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut cur: Option<ChangedFile> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(f) = cur.take() {
                files.push(f);
            }
            // "a/path b/path" — take the b-side path as the new path.
            let path = rest
                .split(" b/")
                .nth(1)
                .map(str::to_string)
                .unwrap_or_else(|| rest.to_string());
            cur = Some(ChangedFile {
                path,
                ranges: Vec::new(),
                deleted: false,
            });
        } else if line.starts_with("+++ ")
            && let Some(f) = cur.as_mut()
        {
            if line == "+++ /dev/null" {
                f.deleted = true;
            } else if let Some(p) = line.strip_prefix("+++ b/") {
                f.path = p.to_string();
            }
        } else if line.starts_with("@@")
            && let (Some(f), Some((start, count))) = (cur.as_mut(), parse_hunk_new_range(line))
            && count > 0
        {
            f.ranges.push((start, start + count - 1));
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

/// Extract the new-side `(start, count)` from a hunk header
/// `@@ -a,b +c,d @@`. A missing count defaults to 1.
fn parse_hunk_new_range(header: &str) -> Option<(usize, usize)> {
    let plus = header.split('+').nth(1)?;
    let token = plus.split([' ', '@']).next()?;
    let mut parts = token.split(',');
    let start: usize = parts.next()?.parse().ok()?;
    let count: usize = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    Some((start, count))
}

fn overlaps(a1: usize, a2: usize, b1: usize, b2: usize) -> bool {
    a1 <= b2 && b1 <= a2
}

/// Map changed files to seed node ids by intersecting changed line ranges with
/// indexed node spans. Whole-file changes seed every symbol in the file.
pub fn seed_nodes(store: &dyn GraphStore, files: &[ChangedFile]) -> Result<SeedSet> {
    let mut seeds: Vec<NodeId> = Vec::new();
    let mut removed_files = Vec::new();
    let mut unresolved_files = Vec::new();

    for f in files {
        if f.deleted {
            removed_files.push(f.path.clone());
            continue;
        }
        let nodes = store.nodes_in_file(&f.path)?;
        if nodes.is_empty() {
            unresolved_files.push(f.path.clone());
            continue;
        }
        if f.ranges.is_empty() {
            seeds.extend(nodes.into_iter().map(|n| n.id));
            continue;
        }
        let mut hit = false;
        for n in &nodes {
            if let Some(s) = n.span
                && f.ranges
                    .iter()
                    .any(|(a, b)| overlaps(s.start_line, s.end_line, *a, *b))
            {
                seeds.push(n.id.clone());
                hit = true;
            }
        }
        if !hit {
            unresolved_files.push(f.path.clone());
        }
    }

    seeds.sort_by(|a, b| a.0.cmp(&b.0));
    seeds.dedup();
    Ok(SeedSet {
        seeds,
        removed_files,
        unresolved_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn parses_new_side_ranges_and_deletions() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,0 +11,3 @@ fn x() {
+a
+b
+c
@@ -40,2 +44,1 @@
+changed
diff --git a/old.rs b/old.rs
deleted file mode 100644
index 333..000
--- a/old.rs
+++ /dev/null
@@ -1,5 +0,0 @@
";
        let files = parse_diff(diff);
        check!(files.len() == 2);
        check!(files[0].path == "src/lib.rs");
        check!(files[0].ranges == vec![(11, 13), (44, 44)]);
        check!(files[0].deleted == false);
        check!(files[1].path == "old.rs");
        check!(files[1].deleted);
        check!(files[1].ranges.is_empty()); // +0,0 deletion -> no new-side range
    }

    #[test]
    fn hunk_without_count_defaults_to_one_line() {
        check!(parse_hunk_new_range("@@ -5 +7 @@") == Some((7, 1)));
        check!(parse_hunk_new_range("@@ -5,2 +7,4 @@ ctx") == Some((7, 4)));
    }

    #[test]
    fn seeds_map_ranges_to_overlapping_spans() {
        use glyphtrail_core::{Node, NodeId, NodeKind, Span};
        let mut store = LadybugStore::open_temp().unwrap();
        let mk = |id: &str, sl: usize, el: usize| Node {
            id: NodeId(id.into()),
            kind: NodeKind::Function,
            name: id.into(),
            qualified_name: id.into(),
            file: "src/lib.rs".into(),
            language: Some("rust".into()),
            span: Some(Span {
                start_byte: 0,
                end_byte: 0,
                start_line: sl,
                end_line: el,
            }),
            doc: None,
            signature: None,
        };
        store
            .insert_graph(&[mk("f1", 1, 10), mk("f2", 20, 30)], &[])
            .unwrap();

        let files = vec![ChangedFile {
            path: "src/lib.rs".into(),
            ranges: vec![(5, 6)],
            deleted: false,
        }];
        let set = seed_nodes(&store, &files).unwrap();
        check!(set.seeds == vec![NodeId("f1".into())]);

        let files = vec![ChangedFile {
            path: "nope.rs".into(),
            ranges: vec![(1, 1)],
            deleted: false,
        }];
        let set = seed_nodes(&store, &files).unwrap();
        check!(set.seeds.is_empty());
        check!(set.unresolved_files == vec!["nope.rs".to_string()]);
    }
}
