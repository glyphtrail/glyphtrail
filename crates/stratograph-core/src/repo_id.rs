//! Stable repository identity (#233).
//!
//! A registry entry's user-chosen name is fragile: it changes when a folder is
//! renamed, two clones of the same repo get different names, and a local
//! project can collide with an unrelated published crate of the same name. A
//! stable id derived from the repo's *forge identity* (its git remote) sidesteps
//! all three — the remote survives folder renames, is identical across clones,
//! and is independent of the package/dir name.
//!
//! Each git remote canonicalizes to `host/owner/repo` (scheme, userinfo, port
//! and `.git` stripped; lowercased), and that string hashes to a UUIDv5 under a
//! fixed stratograph namespace. A repo carries a *set* of ids, all pointing at
//! the same repo — it commonly has several: an origin plus mirrors (GitHub +
//! Codeberg), or a prior identity it migrated from (an old SVN/GitHub URL). The
//! set is the union over all of them, so a dependency referencing *any* of a
//! repo's identities resolves to it.
//!
//! [`repo_ids`] derives ids from the *current* git remotes. Mirrors that aren't
//! configured as git remotes, and pre-migration identities that no longer
//! appear in `git remote`, are user-*declared* and merged in at the registry
//! layer (the union lives on the registry entry, not here).
//!
//! Pure: this turns remote *URLs* into ids. Reading the remotes off disk is the
//! caller's job (the core stays process-free).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Fixed namespace for stratograph repo ids (a constant random UUID), so v5 ids
/// are stable across machines and runs.
const NAMESPACE: Uuid = Uuid::from_u128(0x9f8e_7d6c_5b4a_3928_1706_f5e4_d3c2_b1a0);

/// A stable identity for a repository: the UUIDv5 plus the canonical forge
/// string it was derived from (kept for readability and debugging).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoId {
    /// UUIDv5 of `source` under the stratograph namespace.
    pub id: String,
    /// Canonical forge identity, e.g. `github.com/owner/repo`.
    pub source: String,
}

/// Derive the stable [`RepoId`]s for a repo from its git remote URLs. Remotes
/// that canonicalize to the same forge identity (e.g. fetch and push URLs of one
/// remote) collapse to a single id; the result is sorted and de-duplicated.
pub fn repo_ids(remote_urls: &[String]) -> Vec<RepoId> {
    let mut ids: Vec<RepoId> = remote_urls
        .iter()
        .filter_map(|u| canonicalize_remote(u))
        .map(|source| RepoId {
            id: repo_uuid(&source),
            source,
        })
        .collect();
    ids.sort_by(|a, b| a.source.cmp(&b.source));
    ids.dedup();
    ids
}

/// UUIDv5 of a canonical forge identity under the stratograph namespace.
pub fn repo_uuid(canonical: &str) -> String {
    Uuid::new_v5(&NAMESPACE, canonical.as_bytes()).to_string()
}

/// Canonicalize a git remote URL to `host/owner/repo`: strip the scheme,
/// userinfo, port and a trailing `.git`/`/`, normalise scp-style
/// (`git@host:owner/repo`), and lowercase. Returns `None` for a URL that doesn't
/// resolve to at least `host/owner/repo`.
pub fn canonicalize_remote(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    let had_scheme = trimmed.contains("://");
    // Drop the scheme.
    let after_scheme = trimmed.split("://").last().unwrap_or(trimmed);
    // Drop userinfo (`user@`), which precedes the host.
    let host_path = match after_scheme.split_once('@') {
        Some((user, rest)) if !user.contains('/') => rest,
        _ => after_scheme,
    };

    // Split authority from path. Scp-style (`host:owner/repo`, no scheme) uses
    // `:` as the separator; with a scheme a `:` in the authority is a port.
    let (authority, path) = if let Some((auth, path)) = host_path.split_once('/') {
        // A scheme-less scp URL like `host:owner/repo` lands here with
        // authority `host:owner`; fix it up below.
        (auth, path.to_string())
    } else if let Some((auth, path)) = host_path.split_once(':') {
        // `host:owner/repo` with no `/` before the `:` and none captured above.
        (auth, path.to_string())
    } else {
        return None;
    };

    let (host, path) = match authority.split_once(':') {
        // scp-style `host:owner` -> host=`host`, prepend `owner` to the path.
        Some((host, owner)) if !had_scheme && !owner.is_empty() => {
            (host, format!("{owner}/{path}"))
        }
        // `host:port` with a scheme -> drop the port.
        Some((host, _port)) => (host, path),
        None => (authority, path),
    };

    let canonical = format!("{}/{}", host, path).to_lowercase();
    (canonical.matches('/').count() >= 2).then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn canonicalizes_common_remote_forms() {
        let want = Some("github.com/owner/repo".to_string());
        check!(canonicalize_remote("https://github.com/owner/repo.git") == want);
        check!(canonicalize_remote("https://github.com/owner/repo") == want);
        check!(canonicalize_remote("git@github.com:owner/repo.git") == want);
        check!(canonicalize_remote("ssh://git@github.com/owner/repo") == want);
        check!(canonicalize_remote("git://github.com/owner/repo.git") == want);
        // Case-insensitive, trailing slash tolerated.
        check!(canonicalize_remote("https://GitHub.com/Owner/Repo/") == want);
    }

    #[test]
    fn handles_self_hosted_subgroups_and_ports() {
        check!(
            canonicalize_remote("https://gitlab.com/group/sub/repo.git")
                == Some("gitlab.com/group/sub/repo".to_string())
        );
        // Port (with scheme) is dropped.
        check!(
            canonicalize_remote("https://git.example.com:8443/team/repo.git")
                == Some("git.example.com/team/repo".to_string())
        );
    }

    #[test]
    fn rejects_non_repo_urls() {
        check!(canonicalize_remote("https://github.com") == None);
        check!(canonicalize_remote("not a url") == None);
        check!(canonicalize_remote("") == None);
    }

    #[test]
    fn same_repo_different_url_forms_share_one_id() {
        // A folder rename doesn't touch the remote; ssh and https forms of the
        // same repo must produce the same id.
        let https = repo_uuid(&canonicalize_remote("https://github.com/o/r.git").unwrap());
        let ssh = repo_uuid(&canonicalize_remote("git@github.com:o/r.git").unwrap());
        check!(https == ssh);
        // Stable (UUIDv5) and well-formed.
        check!(https.len() == 36);
    }

    #[test]
    fn repo_ids_dedups_and_keeps_distinct_forges() {
        let ids = repo_ids(&[
            "https://github.com/o/r.git".to_string(),
            "git@github.com:o/r.git".to_string(), // same forge id as above
            "https://gitlab.com/o/r.git".to_string(),
        ]);
        check!(ids.len() == 2);
        check!(ids.iter().any(|i| i.source == "github.com/o/r"));
        check!(ids.iter().any(|i| i.source == "gitlab.com/o/r"));
    }
}
