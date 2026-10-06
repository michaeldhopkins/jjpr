//! Finding the repo and the forge it pushes to: shared by every command that
//! talks to a forge, `jjpr undo` included.

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config;
use crate::forge::remote;
use crate::forge::token as forge_token;
use crate::forge::types::RepoInfo;
use crate::forge::{
    AuthScheme, Forge, ForgeClient, ForgeKind, ForgejoForge, GitHubForge, GitLabForge,
    PaginationStyle,
};

pub struct ResolvedForge {
    pub forge: Box<dyn Forge>,
    pub kind: ForgeKind,
    pub remote_name: String,
    pub repo_info: RepoInfo,
}

/// Resolve the forge to use from config + remotes.
///
/// When `config.forge` is set, it's authoritative: we use that forge kind
/// and resolve the token from `config.forge_token_env` (or the forge's default
/// env var). Errors reflect the config not working, not a detection failure.
///
/// When `config.forge` is not set, we auto-detect from remote URLs.
pub fn resolve_forge(
    remotes: &[crate::jj::GitRemote],
    cfg: &config::Config,
    preferred_remote: Option<&str>,
) -> Result<ResolvedForge> {
    if let Some(kind) = cfg.forge {
        resolve_forge_from_config(
            remotes,
            kind,
            cfg.forge_token_env.as_deref(),
            preferred_remote,
        )
    } else {
        resolve_forge_auto(remotes, preferred_remote)
    }
}

pub fn resolve_forge_from_config(
    remotes: &[crate::jj::GitRemote],
    kind: ForgeKind,
    token_env: Option<&str>,
    preferred_remote: Option<&str>,
) -> Result<ResolvedForge> {
    let env_var = token_env.unwrap_or(kind.token_env_var());
    let token = std::env::var(env_var).ok().filter(|v| !v.is_empty());

    let remote = remote::pick_remote(remotes, preferred_remote)?;
    let host = remote::extract_host(&remote.url);
    let repo_info = remote::parse_url_as(&remote.url, kind).ok_or_else(|| {
        anyhow::anyhow!(
            "could not parse owner/repo from remote '{}' URL: {}",
            remote.name,
            remote.url
        )
    })?;

    let forge = build_forge(kind, host, token, token_env)?;
    Ok(ResolvedForge {
        forge,
        kind,
        remote_name: remote.name.clone(),
        repo_info,
    })
}

pub fn resolve_forge_auto(
    remotes: &[crate::jj::GitRemote],
    preferred_remote: Option<&str>,
) -> Result<ResolvedForge> {
    let (remote_name, kind, repo_info) = remote::resolve_remote(remotes, preferred_remote)?;
    let host = find_remote_host(remotes, &remote_name);
    let forge = build_forge(kind, host, None, None)?;
    Ok(ResolvedForge {
        forge,
        kind,
        remote_name,
        repo_info,
    })
}

pub fn find_remote_host<'a>(
    remotes: &'a [crate::jj::GitRemote],
    remote_name: &str,
) -> Option<&'a str> {
    remotes
        .iter()
        .find(|r| r.name == remote_name)
        .and_then(|r| remote::extract_host(&r.url))
}

pub fn build_forge(
    kind: ForgeKind,
    host: Option<&str>,
    token: Option<String>,
    token_env: Option<&str>,
) -> Result<Box<dyn Forge>> {
    let token = match token {
        Some(t) => t,
        None => forge_token::resolve_token(kind, host, token_env)?,
    };
    match kind {
        ForgeKind::GitHub => {
            let client = ForgeClient::new(
                "https://api.github.com",
                token,
                AuthScheme::Bearer,
                PaginationStyle::LinkHeader,
            );
            Ok(Box::new(GitHubForge::new(client)))
        }
        ForgeKind::GitLab => {
            let gitlab_host = host.unwrap_or("gitlab.com");
            let base_url = format!("https://{gitlab_host}/api/v4");
            let client = ForgeClient::new(
                &base_url,
                token,
                AuthScheme::Bearer,
                PaginationStyle::LinkHeader,
            );
            Ok(Box::new(GitLabForge::new(client)))
        }
        ForgeKind::Forgejo => {
            let host = host.ok_or_else(|| {
                anyhow::anyhow!("could not determine Forgejo host from remote URL")
            })?;
            let base_url = format!("https://{host}/api/v1");
            let client = ForgeClient::new(
                &base_url,
                token,
                AuthScheme::Token,
                PaginationStyle::PageNumber { limit: 50 },
            );
            Ok(Box::new(ForgejoForge::new(client)))
        }
    }
}

pub fn find_repo_root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to get current directory")?;

    let mut path = cwd.as_path();
    loop {
        if path.join(".jj").is_dir() {
            return Ok(path.to_path_buf());
        }
        match path.parent() {
            Some(parent) => path = parent,
            None => {
                // Check if there's a git repo that could be colocated
                let mut check = cwd.as_path();
                loop {
                    if check.join(".git").exists() {
                        anyhow::bail!(
                            "found a git repository but no jj repository. \
                             Run `jj git init --colocate` to set up jj alongside git."
                        );
                    }
                    match check.parent() {
                        Some(parent) => check = parent,
                        None => break,
                    }
                }
                anyhow::bail!(
                    "not a jj repository (or any parent up to /). \
                     Run `jj git init` to create one."
                );
            }
        }
    }
}
