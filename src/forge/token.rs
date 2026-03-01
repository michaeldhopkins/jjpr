use std::process::Command;

use anyhow::Result;

use super::ForgeKind;

/// Resolve an API token for the given forge.
///
/// Fallback chain:
/// 1. `config_env` env var (if set in repo config)
/// 2. Default env var for the forge (`GITHUB_TOKEN`, `GITLAB_TOKEN`, `FORGEJO_TOKEN`)
/// 3. CLI fallback (`gh auth token` for GitHub, `glab auth status -t` for GitLab)
/// 4. Error with a clear message naming the env var to set
pub fn resolve_token(kind: ForgeKind, config_env: Option<&str>) -> Result<String> {
    // 1. Custom env var from config
    if let Some(env_name) = config_env
        && let Ok(val) = std::env::var(env_name)
        && !val.is_empty()
    {
        return Ok(val);
    }

    // 2. Default env var(s)
    for var in &default_env_vars(kind) {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            return Ok(val);
        }
    }

    // 3. CLI fallback
    if let Some(token) = cli_fallback(kind) {
        return Ok(token);
    }

    // 4. Error
    let primary_var = config_env.unwrap_or(kind.token_env_var());
    match kind {
        ForgeKind::GitHub => anyhow::bail!(
            "GitHub token not found. Either:\n  \
             - Run `gh auth login`, or\n  \
             - Set {primary_var} environment variable"
        ),
        ForgeKind::GitLab => anyhow::bail!(
            "GitLab token not found. Either:\n  \
             - Run `glab auth login`, or\n  \
             - Set {primary_var} environment variable"
        ),
        ForgeKind::Forgejo => anyhow::bail!(
            "{primary_var} not set. Generate a token from your Forgejo/Codeberg \
             account settings and export it."
        ),
    }
}

/// Default environment variable names for each forge.
fn default_env_vars(kind: ForgeKind) -> Vec<&'static str> {
    match kind {
        ForgeKind::GitHub => vec!["GITHUB_TOKEN", "GH_TOKEN"],
        ForgeKind::GitLab => vec!["GITLAB_TOKEN"],
        ForgeKind::Forgejo => vec!["FORGEJO_TOKEN"],
    }
}

/// Try to extract a token from the forge's CLI tool.
fn cli_fallback(kind: ForgeKind) -> Option<String> {
    match kind {
        ForgeKind::GitHub => gh_auth_token(),
        ForgeKind::GitLab => glab_auth_token(),
        ForgeKind::Forgejo => None,
    }
}

/// Run `gh auth token` to get the GitHub token from gh's credential store.
fn gh_auth_token() -> Option<String> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Run `glab auth status -t` and parse the token from stderr output.
/// glab prints "Token: glpat-xxx" to stderr.
fn glab_auth_token() -> Option<String> {
    let output = Command::new("glab")
        .args(["auth", "status", "-t"])
        .output()
        .ok()?;

    // glab writes token info to stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Token:") {
            let token = rest.trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_env_vars_github() {
        let vars = default_env_vars(ForgeKind::GitHub);
        assert_eq!(vars, vec!["GITHUB_TOKEN", "GH_TOKEN"]);
    }

    #[test]
    fn test_default_env_vars_gitlab() {
        let vars = default_env_vars(ForgeKind::GitLab);
        assert_eq!(vars, vec!["GITLAB_TOKEN"]);
    }

    #[test]
    fn test_default_env_vars_forgejo() {
        let vars = default_env_vars(ForgeKind::Forgejo);
        assert_eq!(vars, vec!["FORGEJO_TOKEN"]);
    }

    #[test]
    fn test_resolve_token_error_mentions_custom_env() {
        // Use a var name that definitely won't exist in the environment
        let var_name = "JJPR_TEST_NONEXISTENT_TOKEN_42_ZZZZZ";
        let result = resolve_token(ForgeKind::Forgejo, Some(var_name));
        let err = result.expect_err("should fail");
        assert!(
            err.to_string().contains(var_name),
            "error should mention {var_name}: {err}"
        );
    }

    #[test]
    fn test_cli_fallback_forgejo_returns_none() {
        assert!(cli_fallback(ForgeKind::Forgejo).is_none());
    }
}
