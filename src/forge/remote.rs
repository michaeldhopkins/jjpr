use anyhow::Result;

use super::types::RepoInfo;
use super::ForgeKind;
use crate::jj::GitRemote;

/// Parse a GitHub remote URL into owner/repo.
///
/// Supports HTTPS (`https://github.com/owner/repo.git`),
/// SSH (`git@github.com:owner/repo.git`),
/// and GitHub Enterprise subdomains (`company.github.com`).
pub fn parse_github_url(url: &str) -> Option<RepoInfo> {
    parse_url_for_host(url, is_github_host)
}

/// Parse a GitLab remote URL into owner/repo.
///
/// Supports `gitlab.com` and GitLab self-hosted subdomains (`company.gitlab.com`).
/// For nested groups (`group/subgroup/repo`), `owner` contains the full namespace.
pub fn parse_gitlab_url(url: &str) -> Option<RepoInfo> {
    let (host, path) = extract_host_and_path(url)?;
    if !is_gitlab_host(host) {
        return None;
    }
    parse_gitlab_path(path)
}

/// Parse a Forgejo/Codeberg remote URL into owner/repo.
pub fn parse_forgejo_url(url: &str) -> Option<RepoInfo> {
    parse_url_for_host(url, is_forgejo_host)
}

/// Detect the forge type and extract repo info from a remote URL.
pub fn detect_forge(url: &str) -> Option<(ForgeKind, RepoInfo)> {
    if let Some(info) = parse_github_url(url) {
        return Some((ForgeKind::GitHub, info));
    }
    if let Some(info) = parse_gitlab_url(url) {
        return Some((ForgeKind::GitLab, info));
    }
    if let Some(info) = parse_forgejo_url(url) {
        return Some((ForgeKind::Forgejo, info));
    }
    None
}

/// Extract host from a remote URL.
///
/// For `ssh://git@host:port/path` remotes, this strips the SSH port so API
/// clients use the HTTPS host rather than the SSH transport port.
pub fn extract_host(url: &str) -> Option<&str> {
    extract_host_and_path(url).map(|(host, _)| host)
}

fn extract_host_and_path(url: &str) -> Option<(&str, &str)> {
    // SSH: git@host:path
    if let Some(rest) = url.strip_prefix("git@") {
        return rest.split_once(':');
    }
    // SSH: ssh://git@host[:port]/path
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        let (host, path) = rest.split_once('/')?;
        return Some((strip_ssh_port(host), path));
    }
    // HTTPS/HTTP
    for prefix in &["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return rest.split_once('/');
        }
    }
    None
}

fn strip_ssh_port(host: &str) -> &str {
    // IPv6 host
    if host.starts_with('[')
        && let Some(idx) = host.find(']')
    {
        return &host[..=idx];
    }

    if let Some((hostname, port)) = host.rsplit_once(':')
        && !hostname.is_empty()
        && !port.is_empty()
    {
        return hostname;
    }

    host
}

fn parse_url_for_host(url: &str, host_check: fn(&str) -> bool) -> Option<RepoInfo> {
    let (host, path) = extract_host_and_path(url)?;
    if !host_check(host) {
        return None;
    }
    parse_owner_repo(path)
}

fn is_github_host(host: &str) -> bool {
    host == "github.com" || host.ends_with(".github.com")
}

fn is_gitlab_host(host: &str) -> bool {
    host == "gitlab.com" || host.ends_with(".gitlab.com")
}

fn is_forgejo_host(host: &str) -> bool {
    host == "codeberg.org"
}

fn parse_owner_repo(path: &str) -> Option<RepoInfo> {
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    let owner = owner.trim();
    let repo = repo.split('/').next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(RepoInfo {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// GitLab supports nested groups: `group/subgroup/repo`.
/// The last segment is the repo, everything before is the namespace.
fn parse_gitlab_path(path: &str) -> Option<RepoInfo> {
    let path = path.strip_suffix(".git").unwrap_or(path);
    let last_slash = path.rfind('/')?;
    let owner = path[..last_slash].trim();
    let repo = path[last_slash + 1..].trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(RepoInfo {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// Parse owner/repo from a URL for a specific forge kind, ignoring host detection.
/// Used when config explicitly sets the forge type.
pub fn parse_url_as(url: &str, kind: ForgeKind) -> Option<RepoInfo> {
    let (_, path) = extract_host_and_path(url)?;
    match kind {
        ForgeKind::GitLab => parse_gitlab_path(path),
        ForgeKind::GitHub | ForgeKind::Forgejo => parse_owner_repo(path),
    }
}

/// Filter a list of git remotes to supported forge remotes.
pub fn find_forge_remotes(remotes: &[GitRemote]) -> Vec<(String, ForgeKind, RepoInfo)> {
    remotes
        .iter()
        .filter_map(|r| {
            let (kind, info) = detect_forge(&r.url)?;
            Some((r.name.clone(), kind, info))
        })
        .collect()
}

/// Select the appropriate remote. If `preferred` is set, use that; otherwise
/// if there's exactly one supported forge remote, use it; otherwise return an error.
pub fn resolve_remote(
    remotes: &[GitRemote],
    preferred: Option<&str>,
) -> Result<(String, ForgeKind, RepoInfo)> {
    let forge_remotes = find_forge_remotes(remotes);

    if let Some(name) = preferred {
        return forge_remotes
            .into_iter()
            .find(|(n, _, _)| n == name)
            .ok_or_else(|| anyhow::anyhow!("remote '{}' is not a supported forge remote", name));
    }

    match forge_remotes.len() {
        0 => anyhow::bail!(
            "no supported forge remotes found. Either add a supported remote \
             (jj git remote add origin <URL>) or set forge = \"...\" in .jj/jjpr.toml"
        ),
        1 => Ok(forge_remotes.into_iter().next().expect("len checked")),
        _ => {
            let names: Vec<&str> = forge_remotes.iter().map(|(n, _, _)| n.as_str()).collect();
            anyhow::bail!(
                "multiple forge remotes found: {}. Use --remote to specify one.",
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_https_url() {
        let info = parse_github_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_github_https_no_git_suffix() {
        let info = parse_github_url("https://github.com/owner/repo").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_github_ssh_url() {
        let info = parse_github_url("git@github.com:owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_github_ssh_no_git_suffix() {
        let info = parse_github_url("git@github.com:owner/repo").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_github_ssh_protocol_url() {
        let info = parse_github_url("ssh://git@github.com/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_github_ssh_protocol_url_with_port() {
        let info = parse_github_url("ssh://git@github.com:22/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_github_enterprise_subdomain() {
        let info = parse_github_url("https://company.github.com/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_reject_non_github_https() {
        assert!(parse_github_url("https://gitlab.com/owner/repo.git").is_none());
    }

    #[test]
    fn test_reject_non_github_ssh() {
        assert!(parse_github_url("git@gitlab.com:owner/repo.git").is_none());
    }

    #[test]
    fn test_reject_empty_url() {
        assert!(parse_github_url("").is_none());
    }

    // GitLab tests
    #[test]
    fn test_parse_gitlab_https() {
        let info = parse_gitlab_url("https://gitlab.com/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_gitlab_ssh() {
        let info = parse_gitlab_url("git@gitlab.com:owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_gitlab_ssh_protocol_url_with_port() {
        let info = parse_gitlab_url("ssh://git@gitlab.com:22/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_gitlab_nested_groups() {
        let info = parse_gitlab_url("https://gitlab.com/group/subgroup/repo.git").unwrap();
        assert_eq!(info.owner, "group/subgroup");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_gitlab_enterprise_subdomain() {
        let info = parse_gitlab_url("https://company.gitlab.com/team/repo.git").unwrap();
        assert_eq!(info.owner, "team");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_gitlab_rejects_github_url() {
        assert!(parse_gitlab_url("https://github.com/owner/repo.git").is_none());
    }

    // Forgejo/Codeberg tests
    #[test]
    fn test_parse_codeberg_https() {
        let info = parse_forgejo_url("https://codeberg.org/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_codeberg_ssh() {
        let info = parse_forgejo_url("git@codeberg.org:owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_forgejo_rejects_github_url() {
        assert!(parse_forgejo_url("https://github.com/owner/repo.git").is_none());
    }

    // detect_forge tests
    #[test]
    fn test_detect_forge_github() {
        let (kind, info) = detect_forge("git@github.com:me/repo.git").unwrap();
        assert_eq!(kind, ForgeKind::GitHub);
        assert_eq!(info.owner, "me");
    }

    #[test]
    fn test_detect_forge_gitlab() {
        let (kind, info) = detect_forge("git@gitlab.com:me/repo.git").unwrap();
        assert_eq!(kind, ForgeKind::GitLab);
        assert_eq!(info.owner, "me");
    }

    #[test]
    fn test_detect_forge_codeberg() {
        let (kind, info) = detect_forge("https://codeberg.org/me/repo.git").unwrap();
        assert_eq!(kind, ForgeKind::Forgejo);
        assert_eq!(info.owner, "me");
    }

    #[test]
    fn test_detect_forge_unknown() {
        assert!(detect_forge("https://example.com/me/repo.git").is_none());
    }

    // find_forge_remotes tests
    #[test]
    fn test_find_forge_remotes_mixed() {
        let remotes = vec![
            GitRemote {
                name: "origin".to_string(),
                url: "git@github.com:me/myrepo.git".to_string(),
            },
            GitRemote {
                name: "upstream".to_string(),
                url: "https://gitlab.com/other/repo.git".to_string(),
            },
            GitRemote {
                name: "unknown".to_string(),
                url: "https://example.com/foo/bar.git".to_string(),
            },
        ];
        let found = find_forge_remotes(&remotes);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "origin");
        assert_eq!(found[0].1, ForgeKind::GitHub);
        assert_eq!(found[1].0, "upstream");
        assert_eq!(found[1].1, ForgeKind::GitLab);
    }

    // resolve_remote tests
    #[test]
    fn test_resolve_remote_single() {
        let remotes = vec![GitRemote {
            name: "origin".to_string(),
            url: "git@github.com:me/repo.git".to_string(),
        }];
        let (name, kind, info) = resolve_remote(&remotes, None).unwrap();
        assert_eq!(name, "origin");
        assert_eq!(kind, ForgeKind::GitHub);
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_resolve_remote_preferred() {
        let remotes = vec![
            GitRemote {
                name: "origin".to_string(),
                url: "git@github.com:me/repo.git".to_string(),
            },
            GitRemote {
                name: "fork".to_string(),
                url: "git@github.com:other/repo.git".to_string(),
            },
        ];
        let (name, _, info) = resolve_remote(&remotes, Some("fork")).unwrap();
        assert_eq!(name, "fork");
        assert_eq!(info.owner, "other");
    }

    #[test]
    fn test_resolve_remote_no_forge() {
        let remotes = vec![GitRemote {
            name: "origin".to_string(),
            url: "https://example.com/me/repo.git".to_string(),
        }];
        let err = resolve_remote(&remotes, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no supported forge remotes found"), "{msg}");
    }

    #[test]
    fn test_resolve_remote_multiple_no_preference() {
        let remotes = vec![
            GitRemote {
                name: "origin".to_string(),
                url: "git@github.com:me/repo.git".to_string(),
            },
            GitRemote {
                name: "gitlab".to_string(),
                url: "git@gitlab.com:me/repo.git".to_string(),
            },
        ];
        let err = resolve_remote(&remotes, None).unwrap_err();
        assert!(err.to_string().contains("multiple forge remotes"));
    }

    // parse_url_as tests — used when config explicitly sets forge type
    #[test]
    fn test_parse_url_as_github_from_any_host() {
        let info = parse_url_as("https://forgejo.example.com/me/repo.git", ForgeKind::GitHub).unwrap();
        assert_eq!(info.owner, "me");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_url_as_forgejo_from_any_host() {
        let info = parse_url_as("git@git.mycompany.com:team/project.git", ForgeKind::Forgejo).unwrap();
        assert_eq!(info.owner, "team");
        assert_eq!(info.repo, "project");
    }

    #[test]
    fn test_parse_url_as_gitlab_uses_nested_groups() {
        let info = parse_url_as("https://git.mycompany.com/group/sub/repo.git", ForgeKind::GitLab).unwrap();
        assert_eq!(info.owner, "group/sub");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_url_as_invalid_url() {
        assert!(parse_url_as("", ForgeKind::GitHub).is_none());
    }

    #[test]
    fn test_extract_host_strips_ssh_port() {
        let host = extract_host("ssh://git@my.gitlab.dev:2022/me/repo.git").unwrap();
        assert_eq!(host, "my.gitlab.dev");
    }

    #[test]
    fn test_extract_host_preserves_https_port() {
        let host = extract_host("https://my.gitlab.dev:8443/me/repo.git").unwrap();
        assert_eq!(host, "my.gitlab.dev:8443");
    }
}
