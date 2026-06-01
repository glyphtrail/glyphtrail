//! Bake provenance into the binary so a running server can report which build
//! it is (#351): the git commit (with a `-dirty` suffix for an uncommitted
//! working tree) and a UTC build timestamp. Both are surfaced by the `status`
//! tool, since `CARGO_PKG_VERSION` alone (a static `0.1.0`) can't tell two
//! builds apart.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rustc-env=GLYPHTRAIL_GIT_COMMIT={}", git_commit());
    println!(
        "cargo:rustc-env=GLYPHTRAIL_BUILD_TIMESTAMP={}",
        build_timestamp()
    );
    emit_rerun_triggers();
}

/// Re-bake when the checked-out commit changes. Watching only `.git/HEAD` is not
/// enough: on a branch, HEAD stays `ref: refs/heads/<branch>` while it is the ref
/// file (or `packed-refs`) that moves on a new commit. Resolve the real git dir
/// (handles worktrees) and watch HEAD, the ref it points at, and `packed-refs`.
fn emit_rerun_triggers() {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) else {
        return; // No git: the commit is reported as "unknown"; nothing to watch.
    };
    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    if let Ok(content) = std::fs::read_to_string(&head)
        && let Some(reference) = content.strip_prefix("ref:").map(str::trim)
    {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(reference).display()
        );
    }
}

/// `git output`, trimmed, or `None` if git is unavailable, errors, or is empty.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Short HEAD hash, suffixed `-dirty` when the working tree has changes.
/// `unknown` when git is unavailable (e.g. building from a packaged crate).
fn git_commit() -> String {
    match git(&["rev-parse", "--short", "HEAD"]) {
        Some(hash) => {
            let dirty = git(&["status", "--porcelain"]).is_some();
            if dirty { format!("{hash}-dirty") } else { hash }
        }
        None => "unknown".to_string(),
    }
}

fn build_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_utc(secs)
}

/// Format Unix seconds as an RFC 3339 UTC timestamp, no time-crate dependency.
/// Civil date from days via Howard Hinnant's algorithm.
fn rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}
