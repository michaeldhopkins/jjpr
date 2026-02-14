use anyhow::Result;

use super::types::RepoInfo;
use crate::jj::GitRemote;

/// Parse a GitHub remote URL into owner/repo.
///
/// Supports HTTPS (`https://github.com/owner/repo.git`),
/// SSH (`git@github.com:owner/repo.git`),
/// and GitHub Enterprise subdomains (`company.github.com`).
pub fn parse_github_url(url: &str) -> Option<RepoInfo> {
    // SSH: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        if !is_github_host(host) {
            return None;
        }
        return parse_owner_repo(path);
    }

    // SSH: ssh://git@github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        let (host, path) = rest.split_once('/')?;
        if !is_github_host(host) {
            return None;
        }
        return parse_owner_repo(path);
    }

    // HTTPS: https://github.com/owner/repo.git
    for prefix in &["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let (host, path) = rest.split_once('/')?;
            if !is_github_host(host) {
                return None;
            }
            return parse_owner_repo(path);
        }
    }

    None
}

fn is_github_host(host: &str) -> bool {
    host == "github.com" || host.ends_with(".github.com")
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

/// Filter a list of git remotes to only GitHub remotes, returning the parsed RepoInfo.
pub fn find_github_remotes(remotes: &[GitRemote]) -> Vec<(String, RepoInfo)> {
    remotes
        .iter()
        .filter_map(|r| {
            let info = parse_github_url(&r.url)?;
            Some((r.name.clone(), info))
        })
        .collect()
}

/// Select the appropriate remote. If `preferred` is set, use that; otherwise
/// if there's exactly one GitHub remote, use it; otherwise return an error.
pub fn resolve_remote(
    remotes: &[GitRemote],
    preferred: Option<&str>,
) -> Result<(String, RepoInfo)> {
    let github_remotes = find_github_remotes(remotes);

    if let Some(name) = preferred {
        return github_remotes
            .into_iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| anyhow::anyhow!("remote '{}' is not a GitHub remote", name));
    }

    match github_remotes.len() {
        0 => anyhow::bail!(
            "no GitHub remotes found. Add one with: jj git remote add origin https://github.com/OWNER/REPO.git"
        ),
        1 => Ok(github_remotes.into_iter().next().expect("len checked")),
        _ => {
            let names: Vec<&str> = github_remotes.iter().map(|(n, _)| n.as_str()).collect();
            anyhow::bail!(
                "multiple GitHub remotes found: {}. Use --remote to specify one.",
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_https_url() {
        let info = parse_github_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_https_no_git_suffix() {
        let info = parse_github_url("https://github.com/owner/repo").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_ssh_url() {
        let info = parse_github_url("git@github.com:owner/repo.git").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_ssh_no_git_suffix() {
        let info = parse_github_url("git@github.com:owner/repo").unwrap();
        assert_eq!(info.owner, "owner");
        assert_eq!(info.repo, "repo");
    }

    #[test]
    fn test_parse_ssh_protocol_url() {
        let info = parse_github_url("ssh://git@github.com/owner/repo.git").unwrap();
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

    #[test]
    fn test_find_github_remotes() {
        let remotes = vec![
            GitRemote {
                name: "origin".to_string(),
                url: "git@github.com:me/myrepo.git".to_string(),
            },
            GitRemote {
                name: "upstream".to_string(),
                url: "https://gitlab.com/other/repo.git".to_string(),
            },
        ];
        let found = find_github_remotes(&remotes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "origin");
        assert_eq!(found[0].1.owner, "me");
    }

    #[test]
    fn test_resolve_remote_single() {
        let remotes = vec![GitRemote {
            name: "origin".to_string(),
            url: "git@github.com:me/repo.git".to_string(),
        }];
        let (name, info) = resolve_remote(&remotes, None).unwrap();
        assert_eq!(name, "origin");
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
        let (name, info) = resolve_remote(&remotes, Some("fork")).unwrap();
        assert_eq!(name, "fork");
        assert_eq!(info.owner, "other");
    }

    #[test]
    fn test_resolve_remote_no_github() {
        let remotes = vec![GitRemote {
            name: "origin".to_string(),
            url: "https://gitlab.com/me/repo.git".to_string(),
        }];
        let err = resolve_remote(&remotes, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no GitHub remotes found"), "{msg}");
        assert!(msg.contains("jj git remote add"), "should include remediation hint: {msg}");
    }

    #[test]
    fn test_resolve_remote_multiple_no_preference() {
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
        let err = resolve_remote(&remotes, None).unwrap_err();
        assert!(err.to_string().contains("multiple GitHub remotes"));
    }
}
