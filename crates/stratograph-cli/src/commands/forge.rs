//! Forge-API numeric repo IDs (#233 PR2b): an optional, rename-proof identity
//! layer on top of the slug ids.
//!
//! A forge's numeric repo id survives owner/repo renames on the forge, unlike
//! the slug. Resolving it needs the forge's API and a token, so this is
//! best-effort and opt-in: for each remote on a recognised forge that has a
//! token in its well-known env var, query the API for the numeric id and derive
//! a [`stratograph_core::forge_numeric_repo_id`]. No token (or any network/API
//! failure) simply yields no numeric id — the slug ids still stand. Tokens are
//! read from the environment and never logged.
//!
//! Well-known token env vars: `GITHUB_TOKEN` (github.com), `GITLAB_TOKEN`
//! (gitlab.com), `CODEBERG_TOKEN` (codeberg.org / Gitea-Forgejo). For GitHub,
//! when `GITHUB_TOKEN` is unset we fall back to the `gh` CLI (which uses its own
//! auth) — an infrequent shell-out. A config map for arbitrary hosts/var-names
//! is a planned follow-on.

use std::process::Command;

use serde_json::Value;
use stratograph_core::{RepoId, canonicalize_remote, forge_numeric_repo_id};

/// Best-effort forge-API numeric ids for a repo's git remotes. Deterministic:
/// sorted and de-duplicated. Empty when no remote is on a recognised forge with
/// an available token.
pub fn forge_numeric_ids(remote_urls: &[String]) -> Vec<RepoId> {
    let mut ids = Vec::new();
    for url in remote_urls {
        let Some(canonical) = canonicalize_remote(url) else {
            continue;
        };
        // canonical is `host/owner/repo[/sub…]`.
        let mut parts = canonical.splitn(3, '/');
        let (Some(host), Some(owner), Some(repo)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Some(numeric) = fetch_numeric_id(host, owner, repo) {
            ids.push(forge_numeric_repo_id(host, &numeric));
        }
    }
    ids.sort_by(|a, b| a.source.cmp(&b.source));
    ids.dedup();
    ids
}

/// Query a recognised forge's API for `owner/repo`'s numeric id, using the
/// forge's well-known token env var. `None` when the host isn't recognised, no
/// token is set, or the request/parse fails.
fn fetch_numeric_id(host: &str, owner: &str, repo: &str) -> Option<String> {
    let (url, header, value) = match host {
        "github.com" => match std::env::var("GITHUB_TOKEN") {
            Ok(token) => (
                format!("https://api.github.com/repos/{owner}/{repo}"),
                "Authorization",
                format!("Bearer {token}"),
            ),
            // No env token: fall back to the `gh` CLI, which carries its own auth.
            Err(_) => return gh_numeric_id(owner, repo),
        },
        "gitlab.com" => {
            let token = std::env::var("GITLAB_TOKEN").ok()?;
            // GitLab identifies a project by its URL-encoded full path.
            let project = format!("{owner}/{repo}").replace('/', "%2F");
            (
                format!("https://gitlab.com/api/v4/projects/{project}"),
                "PRIVATE-TOKEN",
                token,
            )
        }
        // Gitea / Forgejo. Codeberg is the recognised public host; other
        // self-hosted instances arrive with the config-map follow-on.
        "codeberg.org" => {
            let token = std::env::var("CODEBERG_TOKEN").ok()?;
            (
                format!("https://{host}/api/v1/repos/{owner}/{repo}"),
                "Authorization",
                format!("token {token}"),
            )
        }
        _ => return None,
    };

    let response = ureq::get(&url)
        .set(header, &value)
        .set("Accept", "application/json")
        .set("User-Agent", "stratograph")
        .call()
        .ok()?;
    let json: Value = response.into_json().ok()?;
    json.get("id")
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
}

/// GitHub numeric id via the `gh` CLI (`gh api repos/{owner}/{repo} --jq .id`),
/// the fallback when `GITHUB_TOKEN` isn't set. `None` if `gh` is absent,
/// unauthenticated, or returns a non-numeric result.
fn gh_numeric_id(owner: &str, repo: &str) -> Option<String> {
    let output = Command::new("gh")
        .args(["api", &format!("repos/{owner}/{repo}"), "--jq", ".id"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}
