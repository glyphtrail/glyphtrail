//! Forge-API numeric repo IDs (#233 PR2b): an optional, rename-proof identity
//! layer on top of the slug ids.
//!
//! A forge's numeric repo id survives owner/repo renames on the forge, unlike
//! the slug. Resolving it needs the forge's API and a token, so this is
//! best-effort and opt-in: for each remote on a recognised forge that has a
//! token in its well-known env var (or one mapped by the [`ForgeConfig`]), query
//! the API for the numeric id and derive a [`forge_numeric_repo_id`]. No token
//! (or any network/API failure) simply yields no numeric id — the slug ids
//! still stand. Tokens are read from the environment and never logged.
//!
//! Well-known token env vars: `GITHUB_TOKEN` (github.com), `GITLAB_TOKEN`
//! (gitlab.com), `CODEBERG_TOKEN` (codeberg.org / Gitea-Forgejo). For GitHub,
//! when `GITHUB_TOKEN` is unset we fall back to the `gh` CLI (which uses its own
//! auth) — an infrequent shell-out. The [`ForgeConfig`] maps arbitrary hosts to
//! a token env var and forge kind (for self-hosted instances).

use std::process::Command;

use serde_json::Value;

use crate::config::{ForgeConfig, ForgeKind};
use crate::id::{RepoId, canonicalize_remote, forge_numeric_repo_id};

/// Best-effort forge-API numeric ids for a repo's git remotes, using `config`
/// (host → token-env / forge-kind) layered over built-in host recognition.
/// Deterministic: sorted and de-duplicated. Empty when no remote is on a
/// recognised forge with an available token.
pub fn forge_numeric_ids(remote_urls: &[String], config: &ForgeConfig) -> Vec<RepoId> {
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
        if let Some(numeric) = fetch_numeric_id(config, host, owner, repo) {
            ids.push(forge_numeric_repo_id(host, &numeric));
        }
    }
    ids.sort_by(|a, b| a.source.cmp(&b.source));
    ids.dedup();
    ids
}

/// The forge a host speaks: a config-declared `kind` (for self-hosted instances)
/// wins, else the built-in recognition of the public forges.
fn forge_kind(config: &ForgeConfig, host: &str) -> Option<ForgeKind> {
    config.for_host(host).and_then(|h| h.kind).or(match host {
        "github.com" => Some(ForgeKind::GitHub),
        "gitlab.com" => Some(ForgeKind::GitLab),
        "codeberg.org" => Some(ForgeKind::Gitea),
        _ => None,
    })
}

/// Resolve a token: the config-mapped env var for the host wins, else the
/// kind's well-known env var (when one applies).
fn resolve_token(config: &ForgeConfig, host: &str, well_known: Option<&str>) -> Option<String> {
    if let Some(var) = config.for_host(host).and_then(|h| h.token_env.as_deref())
        && let Ok(token) = std::env::var(var)
    {
        return Some(token);
    }
    well_known.and_then(|w| std::env::var(w).ok())
}

/// Query a host's forge API for `owner/repo`'s numeric id. The forge kind and
/// token come from the config (with built-in recognition + well-known env vars
/// as the fallback), so self-hosted Gitea/GitLab work too. `None` when the host
/// isn't a known forge, no token is available, or the request/parse fails.
fn fetch_numeric_id(config: &ForgeConfig, host: &str, owner: &str, repo: &str) -> Option<String> {
    let (url, header, value) = match forge_kind(config, host)? {
        ForgeKind::GitHub => match resolve_token(config, host, Some("GITHUB_TOKEN")) {
            Some(token) => (
                format!("https://api.github.com/repos/{owner}/{repo}"),
                "Authorization",
                format!("Bearer {token}"),
            ),
            // No token: fall back to the `gh` CLI, which carries its own auth.
            None => return gh_numeric_id(owner, repo),
        },
        ForgeKind::GitLab => {
            let token = resolve_token(config, host, Some("GITLAB_TOKEN"))?;
            // GitLab identifies a project by its URL-encoded full path.
            let project = format!("{owner}/{repo}").replace('/', "%2F");
            (
                format!("https://{host}/api/v4/projects/{project}"),
                "PRIVATE-TOKEN",
                token,
            )
        }
        ForgeKind::Gitea => {
            // Codeberg has a well-known var; self-hosted Gitea uses config token_env.
            let well_known = (host == "codeberg.org").then_some("CODEBERG_TOKEN");
            let token = resolve_token(config, host, well_known)?;
            (
                format!("https://{host}/api/v1/repos/{owner}/{repo}"),
                "Authorization",
                format!("token {token}"),
            )
        }
    };

    let json: Value = ureq::get(&url)
        .header(header, value.as_str())
        .header("Accept", "application/json")
        .header("User-Agent", "glyphtrail")
        .call()
        .ok()?
        .into_body()
        .read_json()
        .ok()?;
    json.get("id")
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
}

/// Best-effort: whether the repo is **private** on its forge, from the forge API
/// (the same endpoint the numeric id comes from). `Some(true/false)` from the first
/// remote that resolves; `None` when no remote is on a recognised forge with an
/// available token, or every request fails — the caller then falls back to
/// host-based inference. Honest by construction: a `Some(false)` means the forge
/// *confirmed* the repo is public, not merely that it's on a public host.
pub fn forge_repo_private(remote_urls: &[String], config: &ForgeConfig) -> Option<bool> {
    for url in remote_urls {
        let Some(canonical) = canonicalize_remote(url) else {
            continue;
        };
        let mut parts = canonical.splitn(3, '/');
        let (Some(host), Some(owner), Some(repo)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Some(private) = fetch_repo_private(config, host, owner, repo) {
            return Some(private);
        }
    }
    None
}

/// Query a host's forge API for whether `owner/repo` is private. Mirrors
/// [`fetch_numeric_id`] (same endpoints/auth); GitHub/Gitea report a `private`
/// boolean, GitLab a `visibility` string (anything but `public` is private).
fn fetch_repo_private(config: &ForgeConfig, host: &str, owner: &str, repo: &str) -> Option<bool> {
    let kind = forge_kind(config, host)?;
    let (url, header, value) = match kind {
        ForgeKind::GitHub => match resolve_token(config, host, Some("GITHUB_TOKEN")) {
            Some(token) => (
                format!("https://api.github.com/repos/{owner}/{repo}"),
                "Authorization",
                format!("Bearer {token}"),
            ),
            None => return gh_repo_private(owner, repo),
        },
        ForgeKind::GitLab => {
            let token = resolve_token(config, host, Some("GITLAB_TOKEN"))?;
            let project = format!("{owner}/{repo}").replace('/', "%2F");
            (
                format!("https://{host}/api/v4/projects/{project}"),
                "PRIVATE-TOKEN",
                token,
            )
        }
        ForgeKind::Gitea => {
            let well_known = (host == "codeberg.org").then_some("CODEBERG_TOKEN");
            let token = resolve_token(config, host, well_known)?;
            (
                format!("https://{host}/api/v1/repos/{owner}/{repo}"),
                "Authorization",
                format!("token {token}"),
            )
        }
    };
    let json: Value = ureq::get(&url)
        .header(header, value.as_str())
        .header("Accept", "application/json")
        .header("User-Agent", "glyphtrail")
        .call()
        .ok()?
        .into_body()
        .read_json()
        .ok()?;
    private_from_response(kind, &json)
}

/// Read the private flag from a forge's repo/project JSON: GitHub/Gitea report a
/// `private` boolean, GitLab a `visibility` string where anything but `public`
/// (i.e. `internal`/`private`) counts as private. Pure — the network-free core of
/// [`fetch_repo_private`], so it's unit-testable against captured responses.
fn private_from_response(kind: ForgeKind, json: &Value) -> Option<bool> {
    match kind {
        ForgeKind::GitHub | ForgeKind::Gitea => json.get("private").and_then(Value::as_bool),
        ForgeKind::GitLab => json
            .get("visibility")
            .and_then(Value::as_str)
            .map(|v| !v.eq_ignore_ascii_case("public")),
    }
}

/// GitHub repo private flag via the `gh` CLI (`gh api repos/{owner}/{repo} --jq
/// .private`), the fallback when `GITHUB_TOKEN` isn't set.
fn gh_repo_private(owner: &str, repo: &str) -> Option<bool> {
    let output = Command::new("gh")
        .args(["api", &format!("repos/{owner}/{repo}"), "--jq", ".private"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;
    use serde_json::json;

    #[test]
    fn github_private_flag_drives_visibility() {
        // GitHub/Gitea expose a boolean `private`.
        check!(private_from_response(ForgeKind::GitHub, &json!({"private": true})) == Some(true));
        check!(private_from_response(ForgeKind::GitHub, &json!({"private": false})) == Some(false));
        check!(private_from_response(ForgeKind::Gitea, &json!({"private": true})) == Some(true));
        // Absent/non-bool field → unknown, so the caller falls back to inference.
        check!(private_from_response(ForgeKind::GitHub, &json!({"id": 42})) == None);
    }

    #[test]
    fn gitlab_visibility_maps_non_public_to_private() {
        // GitLab uses a `visibility` string; only `public` is public.
        check!(
            private_from_response(ForgeKind::GitLab, &json!({"visibility": "public"}))
                == Some(false)
        );
        check!(
            private_from_response(ForgeKind::GitLab, &json!({"visibility": "internal"}))
                == Some(true)
        );
        check!(
            private_from_response(ForgeKind::GitLab, &json!({"visibility": "private"}))
                == Some(true)
        );
        // Case-insensitive, and a missing field stays unknown.
        check!(
            private_from_response(ForgeKind::GitLab, &json!({"visibility": "Public"}))
                == Some(false)
        );
        check!(private_from_response(ForgeKind::GitLab, &json!({})) == None);
    }
}
