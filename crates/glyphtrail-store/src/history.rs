//! Symbol-level git history (#449): the commits that touched a symbol, each
//! flagged for whether it is already in HEAD, with the branches that contain it
//! and (optionally) an open forge PR.
//!
//! This answers "I thought I fixed this — where did the fix go?": a fix parked
//! on an unmerged branch shows up as a commit with `in_head: false` plus the
//! branch (and PR) carrying it.
//!
//! Two git passes are merged:
//! - `git log -L<start>,<end>:<file>` — precise history of the symbol's lines,
//!   but only along HEAD's ancestry (`-L` cannot be combined with `--all`); and
//! - `git log --all -S<name> -- <file>` — the pickaxe across *all* refs, which
//!   surfaces off-branch commits (and a symbol no longer present at HEAD).
//!
//! Without a span only the pickaxe runs. Git is shelled out to keep the
//! dependency surface light (like `changeset`); because containment is decided
//! by `git merge-base --is-ancestor` — whose exit code *is* the answer — these
//! helpers read git's status directly rather than failing on a non-zero exit.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

const SEP: char = '\x1f';

/// One commit that touched a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolCommit {
    pub hash: String,
    /// Abbreviated hash (`%h`).
    pub short: String,
    /// Commit date (`%ad`, `--date=short`).
    pub date: String,
    pub author: String,
    pub subject: String,
    /// Whether the commit is an ancestor of HEAD (already on the current branch).
    pub in_head: bool,
    /// Short refnames of branches containing the commit (computed for off-HEAD
    /// commits; empty otherwise).
    pub branches: Vec<String>,
    /// An open PR carrying the commit's branch — only with `prs` and a match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrRef>,
}

/// An open forge pull request, from `gh pr list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: String,
}

/// Options for [`symbol_history`].
#[derive(Debug, Clone, Copy)]
pub struct HistoryOpts {
    /// Enrich off-HEAD commits with an open forge PR (needs `gh`; best-effort).
    pub prs: bool,
    /// Cap on the number of commits per git pass.
    pub limit: usize,
}

impl Default for HistoryOpts {
    fn default() -> Self {
        Self {
            prs: false,
            limit: 50,
        }
    }
}

/// The commits that touched `name` in `file`, newest first, across all refs.
///
/// Returns an empty vec (not an error) outside a git repository or when nothing
/// matched, so a read-only query degrades gracefully.
pub fn symbol_history(
    root: &Path,
    file: &str,
    span: Option<(usize, usize)>,
    name: &str,
    opts: &HistoryOpts,
) -> Vec<SymbolCommit> {
    let mut raw = Vec::new();
    // Precise in-HEAD span history (only when the symbol has a span at HEAD).
    if let Some((start, end)) = span {
        raw.extend(run_log(root, &span_args(start, end, file), opts.limit));
    }
    // All-refs pickaxe by name — the off-branch / "lost fix" surface.
    raw.extend(run_log(root, &pickaxe_args(name, file), opts.limit));

    // Merge: newest first, one entry per commit.
    raw.sort_by_key(|c| std::cmp::Reverse(c.committed_at));
    raw.dedup_by(|a, b| a.hash == b.hash);

    raw.into_iter()
        .map(|r| {
            let in_head = is_ancestor(root, &r.hash);
            // Branches matter for the off-HEAD commits (where the fix lives); skip
            // the extra git call for ones already on HEAD.
            let branches = if in_head {
                Vec::new()
            } else {
                branches_containing(root, &r.hash)
            };
            let pr = (opts.prs && !in_head)
                .then(|| branches.iter().find_map(|b| open_pr_for_branch(root, b)))
                .flatten();
            SymbolCommit {
                hash: r.hash,
                short: r.short,
                date: r.date,
                author: r.author,
                subject: r.subject,
                in_head,
                branches,
                pr,
            }
        })
        .collect()
}

/// A commit row before HEAD-containment/branch annotation.
struct RawCommit {
    hash: String,
    short: String,
    committed_at: i64,
    date: String,
    author: String,
    subject: String,
}

fn pretty() -> String {
    format!("--pretty=format:%H{SEP}%h{SEP}%ct{SEP}%ad{SEP}%an{SEP}%s")
}

/// `git log -L<start>,<end>:<file>` along HEAD (no `--all`; `-L` forbids it).
fn span_args(start: usize, end: usize, file: &str) -> Vec<String> {
    vec![
        "log".into(),
        "-s".into(),
        "--date=short".into(),
        pretty(),
        format!("-L{start},{end}:{file}"),
    ]
}

/// `git log --all -S<name> -- <file>` — the pickaxe across all refs.
fn pickaxe_args(name: &str, file: &str) -> Vec<String> {
    vec![
        "log".into(),
        "--all".into(),
        "-s".into(),
        "--date=short".into(),
        pretty(),
        format!("-S{name}"),
        "--".into(),
        file.into(),
    ]
}

/// Run a `git log` invocation under `root` and parse it; empty on any failure.
fn run_log(root: &Path, args: &[String], limit: usize) -> Vec<RawCommit> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("log")
        .arg(format!("-n{limit}"))
        // `args[0]` is already "log"; pass the rest after our `-n`.
        .args(&args[1..])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_log(&String::from_utf8_lossy(&out.stdout))
}

fn parse_log(text: &str) -> Vec<RawCommit> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(SEP);
            Some(RawCommit {
                hash: f.next()?.trim().to_string(),
                short: f.next()?.to_string(),
                committed_at: f.next()?.trim().parse().ok()?,
                date: f.next()?.to_string(),
                author: f.next()?.to_string(),
                subject: f.next().unwrap_or("").to_string(),
            })
        })
        .filter(|c| !c.hash.is_empty())
        .collect()
}

/// Whether `commit` is an ancestor of HEAD (`git merge-base --is-ancestor`).
fn is_ancestor(root: &Path, commit: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Short refnames of branches (local + remote) containing `commit`.
fn branches_containing(root: &Path, commit: &str) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "branch",
            "--all",
            "--contains",
            commit,
            "--format=%(refname:short)",
        ])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("HEAD")) // drop `origin/HEAD`
        .map(str::to_string)
        .collect()
}

/// The first open PR whose head is `branch`, via `gh`. Best-effort: missing
/// `gh`, no auth, or no match all yield `None`.
fn open_pr_for_branch(root: &Path, branch: &str) -> Option<PrRef> {
    // `gh` wants the local branch name, not a remote-qualified refname.
    let head = branch.rsplit_once('/').map(|(_, b)| b).unwrap_or(branch);
    let out = Command::new("gh")
        .current_dir(root)
        .args([
            "pr",
            "list",
            "--head",
            head,
            "--state",
            "open",
            "--json",
            "number,url,title,state",
            "--limit",
            "1",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let arr: Vec<PrRef> = serde_json::from_slice(&out.stdout).ok()?;
    arr.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn parse_log_reads_unit_separated_rows() {
        let text = format!(
            "abc123{SEP}abc{SEP}1700000000{SEP}2023-11-14{SEP}Ada{SEP}fix items_segments\n\
             def456{SEP}def{SEP}1699999999{SEP}2023-11-13{SEP}Bob{SEP}unrelated"
        );
        let got = parse_log(&text);
        check!(got.len() == 2);
        check!(got[0].hash == "abc123");
        check!(got[0].committed_at == 1_700_000_000);
        check!(got[0].subject == "fix items_segments");
        check!(got[1].author == "Bob");
    }

    #[test]
    fn span_and_pickaxe_args_shape() {
        check!(span_args(10, 20, "src/a.rs").last().unwrap() == "-L10,20:src/a.rs");
        let pk = pickaxe_args("square", "src/a.rs");
        check!(pk.contains(&"--all".to_string()));
        check!(pk.contains(&"-Ssquare".to_string()));
        check!(pk.last().unwrap() == "src/a.rs");
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            // Identity inline so the test needs no global git config.
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    // End-to-end (#449): a commit that touches the symbol on an unmerged branch
    // is found and flagged `in_head: false` with its branch; the base commit on
    // the current branch is `in_head: true`.
    #[test]
    fn symbol_history_flags_off_head_branch_commit() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("gt-history-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();

        git(&dir, &["init", "-b", "main", "-q"]);
        std::fs::write(dir.join("app.py"), "def square(x):\n    return x * x\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "add square"]);

        // A fix on an unmerged branch that adds a *use* of `square` (changes the
        // occurrence count, so the pickaxe finds it).
        git(&dir, &["checkout", "-q", "-b", "fix/use-square"]);
        std::fs::write(
            dir.join("app.py"),
            "def square(x):\n    return x * x\n\nprint(square(3))\n",
        )
        .unwrap();
        git(&dir, &["commit", "-q", "-am", "call square"]);
        git(&dir, &["checkout", "-q", "main"]);

        let commits = symbol_history(
            &dir,
            "app.py",
            Some((1, 2)),
            "square",
            &HistoryOpts::default(),
        );

        let fix = commits.iter().find(|c| c.subject == "call square");
        check!(fix.is_some(), "the off-branch fix commit should be found");
        let fix = fix.unwrap();
        check!(fix.in_head == false);
        check!(fix.branches.iter().any(|b| b.contains("fix/use-square")));

        let base = commits.iter().find(|c| c.subject == "add square");
        check!(base.is_some());
        check!(base.unwrap().in_head == true);

        std::fs::remove_dir_all(&dir).ok();
    }
}
