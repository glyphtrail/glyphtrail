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
//! fixed glyphtrail namespace. A repo carries a *set* of ids, all pointing at
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

/// Fixed namespace for glyphtrail repo ids (a constant random UUID), so v5 ids
/// are stable across machines and runs.
const NAMESPACE: Uuid = Uuid::from_u128(0x9f8e_7d6c_5b4a_3928_1706_f5e4_d3c2_b1a0);

/// A stable identity for a repository: the UUIDv5 plus the canonical forge
/// string it was derived from (kept for readability and debugging).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoId {
    /// UUIDv5 of `source` under the glyphtrail namespace.
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

/// UUIDv5 of a canonical forge identity under the glyphtrail namespace.
pub fn repo_uuid(canonical: &str) -> String {
    Uuid::new_v5(&NAMESPACE, canonical.as_bytes()).to_string()
}

/// A stable id from a forge's *numeric* repo id (GitHub/Gitea/GitLab), which
/// survives forge-side renames — the numeric id is stable even when the
/// `owner/repo` slug changes. The canonical `source` is `host#numeric` (e.g.
/// `codeberg.org#1982264`), distinct from the slug form `host/owner/repo`.
pub fn forge_numeric_repo_id(host: &str, numeric_id: &str) -> RepoId {
    let source = format!("{}#{}", host.to_lowercase(), numeric_id);
    RepoId {
        id: repo_uuid(&source),
        source,
    }
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
    // Some remotes wrap the authority to carry a port in scp form, e.g.
    // `[git@host:port]:owner/repo`. Note the brackets, then drop userinfo
    // (`user@`, which precedes the host) and the brackets themselves.
    let had_brackets = after_scheme.contains('[');
    let host_path = match after_scheme.split_once('@') {
        Some((user, rest)) if !user.contains('/') => rest,
        _ => after_scheme,
    };
    let host_path = host_path.replace(['[', ']'], "");

    // Tokenize on both `/` and `:` so URL (`host/owner/repo`), scp
    // (`host:owner/repo`), and ported forms (`host:port/owner/repo`,
    // `[host:port]:owner/repo`) all decompose to host + path segments.
    let mut segs = host_path.split(['/', ':']).filter(|s| !s.is_empty());
    let host = segs.next()?.to_lowercase();
    let rest: Vec<&str> = segs.collect();

    // A leading all-digit segment is a port — but only where a port is
    // syntactically possible (a scheme, or a bracketed authority). Without
    // either, `host:123/repo` is scp-style and `123` is the owner, so it stays.
    let port_possible = had_scheme || had_brackets;
    let path_segs = match rest.split_first() {
        Some((first, tail))
            if port_possible && !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) =>
        {
            tail
        }
        _ => &rest[..],
    };

    if path_segs.len() < 2 {
        return None; // need at least owner/repo
    }
    Some(format!("{host}/{}", path_segs.join("/")).to_lowercase())
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
        // ssh:// with a non-standard port -> port dropped.
        check!(
            canonicalize_remote("ssh://git@git.internal.example:2222/team/svc.git")
                == Some("git.internal.example/team/svc".to_string())
        );
        // Bracketed scp form carrying a port (`[user@host:port]:owner/repo`),
        // which previously leaked `port]:` into the path.
        check!(
            canonicalize_remote("[git@git.internal.example:2222]:team/svc.git")
                == Some("git.internal.example/team/svc".to_string())
        );
        // ...including a hyphenated subgroup-style owner.
        check!(
            canonicalize_remote("[git@git.internal.example:2222]:sub-group/svc.git")
                == Some("git.internal.example/sub-group/svc".to_string())
        );
        // Plain scp with no scheme/brackets: a numeric owner is NOT a port.
        check!(
            canonicalize_remote("git@github.com:123/repo.git")
                == Some("github.com/123/repo".to_string())
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
    fn forge_numeric_id_is_stable_and_rename_proof() {
        let a = forge_numeric_repo_id("codeberg.org", "1982264");
        check!(a.source == "codeberg.org#1982264");
        check!(a.id.len() == 36);
        // Same forge + numeric => same id regardless of any slug rename.
        check!(a == forge_numeric_repo_id("Codeberg.org", "1982264"));
        // Distinct from the slug id for the same repo.
        check!(a.id != repo_uuid("codeberg.org/glyphtrail/glyphtrail"));
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
