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
use crate::error::ForgeIdError;
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

/// One repo discovered on a forge account (#nnn): enough to dedup against the
/// registry, decide visibility, and clone it. `numeric_id` is the forge's stable
/// id (`None` if the API omitted it); `ssh_url`/`clone_url` are the SSH and HTTPS
/// clone URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    pub host: String,
    pub owner: String,
    pub name: String,
    pub numeric_id: Option<String>,
    pub ssh_url: String,
    pub clone_url: String,
    pub private: bool,
    pub fork: bool,
    pub archived: bool,
}

/// Which repos account discovery returns. **Owned** repos are always included;
/// the rest are opt-in, so a stray org membership (e.g. the EpicGames org the
/// Unreal EULA enrolls you in) can't drag in thousands of repos you never wrote.
#[derive(Debug, Clone, Default)]
pub struct ListOpts {
    pub include_forks: bool,
    pub include_archived: bool,
    /// Specific org logins to include (case-insensitive), e.g. your work orgs.
    pub orgs: Vec<String>,
    /// Include *every* org you're a member of (the broad, footgun-y set).
    pub all_orgs: bool,
    /// Include repos you're only a collaborator on (owned by others).
    pub collaborator: bool,
}

/// Every repo on the configured account for `host` that `opts` selects, across
/// pages. GitHub only for now: with `GITHUB_TOKEN` it calls the REST API directly,
/// else it shells out to `gh` (which carries its own auth). Owned repos always;
/// org repos only for `--orgs`/`--all-orgs`; collaborator repos only for
/// `--collaborator`. Errors when no credentials are available, the host isn't
/// GitHub, or the API/network fails — so the caller can tell "found nothing" from
/// "couldn't ask".
pub fn list_account_repos(
    config: &ForgeConfig,
    host: &str,
    opts: &ListOpts,
) -> Result<Vec<RemoteRepo>, ForgeIdError> {
    match forge_kind(config, host) {
        Some(ForgeKind::GitHub) => {}
        Some(_) => {
            return Err(ForgeIdError::Discovery {
                message: format!("account discovery is GitHub-only for now (got {host})"),
            });
        }
        None => {
            return Err(ForgeIdError::Discovery {
                message: format!("unrecognized forge host {host}"),
            });
        }
    }
    let token = resolve_token(config, host, Some("GITHUB_TOKEN"));
    // One query per affiliation, so each repo is attributable (a combined
    // affiliation list returns a union with no per-repo source).
    let fetch = |affiliation: &str| -> Result<Vec<RemoteRepo>, ForgeIdError> {
        match &token {
            Some(t) => github_repos_by_affiliation(t, affiliation),
            None => gh_repos_by_affiliation(affiliation),
        }
    };

    let mut repos = fetch("owner")?; // always
    if opts.all_orgs || !opts.orgs.is_empty() {
        let mut org = fetch("organization_member")?;
        if !opts.all_orgs {
            let want: std::collections::HashSet<String> =
                opts.orgs.iter().map(|o| o.to_ascii_lowercase()).collect();
            org.retain(|r| want.contains(&r.owner.to_ascii_lowercase()));
        }
        repos.extend(org);
    }
    if opts.collaborator {
        repos.extend(fetch("collaborator")?);
    }
    // De-dup by canonical identity, keeping the first (owned > org > collaborator).
    let mut seen = std::collections::HashSet::new();
    repos.retain(|r| seen.insert(format!("{}/{}/{}", r.host, r.owner, r.name)));
    Ok(filter_repos(repos, opts))
}

/// GitHub `/user/repos` for a single `affiliation` over the REST API with a bearer
/// token, paginated by `?page=N` until a short (< 100) page.
fn github_repos_by_affiliation(
    token: &str,
    affiliation: &str,
) -> Result<Vec<RemoteRepo>, ForgeIdError> {
    let mut all = Vec::new();
    // Cap pages so a pathological/looping response can't spin forever (100 pages ×
    // 100 = 10k repos, well beyond any real account).
    for page in 1..=100u32 {
        // `affiliation` and `type` are mutually exclusive here (the API 422s if both
        // are set); affiliation alone already returns forks + archived, trimmed
        // client-side per `opts`.
        let url = format!(
            "https://api.github.com/user/repos?per_page=100&page={page}&affiliation={affiliation}"
        );
        let json: Value = ureq::get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "glyphtrail")
            .call()
            .map_err(|e| ForgeIdError::Discovery {
                message: format!("GitHub API request failed: {e}"),
            })?
            .into_body()
            .read_json()
            .map_err(|e| ForgeIdError::Discovery {
                message: format!("GitHub API response was not JSON: {e}"),
            })?;
        let page_len = json.as_array().map(Vec::len).unwrap_or(0);
        all.extend(repos_from_json(&json));
        if page_len < 100 {
            break; // last page
        }
    }
    Ok(all)
}

/// GitHub `/user/repos` for a single `affiliation` via the `gh` CLI (`gh api
/// --paginate … --jq '.[]'`), the fallback when `GITHUB_TOKEN` isn't set. `gh`
/// follows pagination and emits one repo object per line (NDJSON).
fn gh_repos_by_affiliation(affiliation: &str) -> Result<Vec<RemoteRepo>, ForgeIdError> {
    let output = Command::new("gh")
        .args([
            "api",
            "--paginate",
            &format!("user/repos?per_page=100&affiliation={affiliation}"),
            "--jq",
            ".[]",
        ])
        .output()
        .map_err(|e| ForgeIdError::Discovery {
            message: format!("could not run `gh` (install it or set GITHUB_TOKEN): {e}"),
        })?;
    if !output.status.success() {
        return Err(ForgeIdError::Discovery {
            message: format!(
                "`gh api user/repos` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let mut objects = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty()
            && let Ok(v) = serde_json::from_str::<Value>(line)
        {
            objects.push(v);
        }
    }
    Ok(repos_from_json(&Value::Array(objects)))
}

/// Parse a GitHub `/user/repos` JSON array into [`RemoteRepo`]s — the network-free
/// core of discovery, so it's unit-testable. Skips entries missing the
/// `owner/name` full name; absent booleans default to `false`.
pub fn repos_from_json(json: &Value) -> Vec<RemoteRepo> {
    let Some(arr) = json.as_array() else {
        return Vec::new();
    };
    arr.iter().filter_map(repo_from_value).collect()
}

fn repo_from_value(v: &Value) -> Option<RemoteRepo> {
    let (owner, name) = v
        .get("full_name")
        .and_then(Value::as_str)?
        .split_once('/')?;
    let str_field = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let bool_field = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    Some(RemoteRepo {
        host: "github.com".to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
        numeric_id: v.get("id").and_then(Value::as_i64).map(|n| n.to_string()),
        ssh_url: str_field("ssh_url"),
        clone_url: str_field("clone_url"),
        private: bool_field("private"),
        fork: bool_field("fork"),
        archived: bool_field("archived"),
    })
}

fn filter_repos(repos: Vec<RemoteRepo>, opts: &ListOpts) -> Vec<RemoteRepo> {
    repos
        .into_iter()
        .filter(|r| opts.include_forks || !r.fork)
        .filter(|r| opts.include_archived || !r.archived)
        .collect()
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
    fn repos_from_json_parses_and_filters() {
        let page = json!([
            {"full_name": "octo/app", "ssh_url": "git@github.com:octo/app.git",
             "clone_url": "https://github.com/octo/app.git", "id": 12, "private": true,
             "fork": false, "archived": false},
            {"full_name": "octo/forked", "ssh_url": "git@github.com:octo/forked.git",
             "clone_url": "https://github.com/octo/forked.git", "id": 34, "private": false,
             "fork": true, "archived": false},
            {"full_name": "octo/old", "ssh_url": "git@github.com:octo/old.git",
             "clone_url": "https://github.com/octo/old.git", "id": 56, "private": false,
             "fork": false, "archived": true},
            {"no_full_name": true}, // skipped — missing owner/name
        ]);
        let all = repos_from_json(&page);
        check!(all.len() == 3); // the malformed entry is dropped
        let app = &all[0];
        check!(app.owner == "octo" && app.name == "app");
        check!(app.numeric_id.as_deref() == Some("12"));
        check!(app.private && !app.fork && !app.archived);
        check!(app.ssh_url == "git@github.com:octo/app.git");

        // Default opts drop forks and archived.
        let lean = filter_repos(
            repos_from_json(&page),
            &ListOpts {
                include_forks: false,
                include_archived: false,
                ..Default::default()
            },
        );
        check!(lean.len() == 1);
        check!(lean[0].name == "app");

        // Broad opts keep everything.
        let broad = filter_repos(
            repos_from_json(&page),
            &ListOpts {
                include_forks: true,
                include_archived: true,
                ..Default::default()
            },
        );
        check!(broad.len() == 3);
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
