use std::process::Command;

use anyhow::Result;

use super::ForgeKind;

/// Resolve an API token for the given forge.
///
/// `host` is the forge host taken from the remote URL. The GitLab CLI fallback
/// passes it to glab so a user logged in to several instances gets the token
/// for the one this repo lives on.
///
/// Fallback chain:
/// 1. `config_env` env var (if set in repo config)
/// 2. Default env var for the forge (`GITHUB_TOKEN`, `GITLAB_TOKEN`, `FORGEJO_TOKEN`)
/// 3. CLI fallback (`gh auth token` for GitHub, `glab auth status -t` for GitLab)
/// 4. Error with a clear message naming the env var to set
pub fn resolve_token(
    kind: ForgeKind,
    host: Option<&str>,
    config_env: Option<&str>,
) -> Result<String> {
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
    if let Some(token) = cli_fallback(kind, host) {
        return Ok(token);
    }

    // 4. Error
    Err(missing_token_error(kind, host, config_env))
}

fn missing_token_error(
    kind: ForgeKind,
    host: Option<&str>,
    config_env: Option<&str>,
) -> anyhow::Error {
    let primary_var = config_env.unwrap_or(kind.token_env_var());
    match kind {
        ForgeKind::GitHub => anyhow::anyhow!(
            "GitHub token not found. Either:\n  \
             - Run `gh auth login`, or\n  \
             - Set {primary_var} environment variable"
        ),
        ForgeKind::GitLab => anyhow::anyhow!(
            "GitLab token not found. Either:\n  \
             - Run `{}`, or\n  \
             - Set {primary_var} environment variable",
            glab_login_hint(host)
        ),
        ForgeKind::Forgejo => anyhow::anyhow!(
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
fn cli_fallback(kind: ForgeKind, host: Option<&str>) -> Option<String> {
    match kind {
        ForgeKind::GitHub => gh_auth_token(),
        ForgeKind::GitLab => glab_auth_token(host),
        ForgeKind::Forgejo => None,
    }
}

/// Run `gh auth token` to get the GitHub token from gh's credential store.
fn gh_auth_token() -> Option<String> {
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Run `glab auth status -t` and parse the token from its stderr.
///
/// glab has no `auth token` command, so its human-readable status output is
/// the only interface. See `parse_glab_status_token` for the formats handled.
fn glab_auth_token(host: Option<&str>) -> Option<String> {
    glab_auth_token_with(|| Command::new("glab"), host)
}

fn glab_auth_token_with(glab: impl Fn() -> Command, host: Option<&str>) -> Option<String> {
    // glab prints the stored token before its own API call refreshes an
    // expired OAuth token, so a single `--show-token` run can hand back a
    // token GitLab rejects with 401 (seen with glab 1.117). The first run
    // does the refresh; its output is ignored, and so is its exit status,
    // since the read below reports whatever state it left behind.
    let _ = glab().args(glab_status_args(host)).output();

    let mut args = glab_status_args(host);
    args.push("--show-token");
    let output = glab().args(args).output().ok()?;
    parse_glab_status_token(&String::from_utf8_lossy(&output.stderr))
}

/// Without `--hostname`, glab reports every instance it is logged in to (or
/// the one its own remote detection picks), and the first token printed may
/// belong to a different instance than this repo's.
fn glab_status_args(host: Option<&str>) -> Vec<&str> {
    let mut args = vec!["auth", "status"];
    if let Some(host) = host {
        args.extend(["--hostname", host]);
    }
    args
}

fn glab_login_hint(host: Option<&str>) -> String {
    match host {
        Some(host) => format!("glab auth login --hostname {host}"),
        None => "glab auth login".to_string(),
    }
}

/// Extract the token from `glab auth status --show-token` output.
///
/// The line has changed shape across glab releases:
/// - `Token: <token>` (old releases)
/// - `✓ Token found: <token>`
/// - `✓ Token found in <storage>: <token>` (glab 1.111+, where `<storage>` is
///   e.g. `operating system keyring` or `configuration file (plaintext)`)
///
/// A masked value (all `*`) means glab ignored `--show-token`; treat it as no
/// token rather than sending asterisks to the API.
fn parse_glab_status_token(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        // glab colors its status icon when it thinks it is on a terminal
        // (or CLICOLOR_FORCE is set), so drop escapes before skipping it.
        let line = strip_ansi(line);
        let trimmed = line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        let token = if let Some(rest) = trimmed.strip_prefix("Token:") {
            rest
        } else {
            let rest = trimmed.strip_prefix("Token found")?;
            match rest.strip_prefix(':') {
                Some(token) => token,
                None if rest.starts_with(" in ") => rest.split_once(": ")?.1,
                None => return None,
            }
        };
        let token = token.trim();
        (!token.is_empty() && !token.chars().all(|c| c == '*')).then(|| token.to_string())
    })
}

/// Remove CSI escape sequences (`ESC [ ... <letter>`), which is all glab emits.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
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
        // Temporarily clear default env vars so the test reaches the error path
        let saved = std::env::var("FORGEJO_TOKEN").ok();
        // SAFETY: test is single-threaded for this env var; restored immediately after
        unsafe { std::env::remove_var("FORGEJO_TOKEN") };

        let var_name = "JJPR_TEST_NONEXISTENT_TOKEN_42_ZZZZZ";
        let result = resolve_token(ForgeKind::Forgejo, None, Some(var_name));

        // Restore
        if let Some(val) = saved {
            unsafe { std::env::set_var("FORGEJO_TOKEN", val) };
        }

        let err = result.expect_err("should fail");
        assert!(
            err.to_string().contains(var_name),
            "error should mention {var_name}: {err}"
        );
    }

    #[test]
    fn test_cli_fallback_forgejo_returns_none() {
        assert!(cli_fallback(ForgeKind::Forgejo, None).is_none());
    }

    fn parse(stderr: &str) -> Option<String> {
        parse_glab_status_token(stderr)
    }

    #[test]
    fn test_parse_glab_oldest_format() {
        assert_eq!(
            parse("  Token: glpat-abc123").as_deref(),
            Some("glpat-abc123")
        );
    }

    #[test]
    fn test_parse_glab_token_found_format() {
        let stderr = "gitlab.com\n  ✓ Logged in to gitlab.com as me (keyring)\n  ✓ Token found: glpat-abc123\n";
        assert_eq!(parse(stderr).as_deref(), Some("glpat-abc123"));
    }

    // glab 1.111+ (issue #13): the storage location sits between "Token found"
    // and the colon.
    #[test]
    fn test_parse_glab_token_found_in_storage_formats() {
        for storage in [
            "operating system keyring",
            "configuration file (plaintext)",
            "environment variable GITLAB_TOKEN",
        ] {
            let stderr = format!(
                "gitlab.com\n  ✓ Logged in to gitlab.com as me (keyring)\n  \
                 ✓ Token found in {storage}: glpat-abc123\n"
            );
            assert_eq!(
                parse(&stderr).as_deref(),
                Some("glpat-abc123"),
                "storage: {storage}"
            );
        }
    }

    #[test]
    fn test_parse_glab_colored_icon() {
        let stderr = "  \u{1b}[32m✓\u{1b}[0m Token found in operating system keyring: glpat-abc123";
        assert_eq!(parse(stderr).as_deref(), Some("glpat-abc123"));
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("\u{1b}[1;32m✓\u{1b}[0m ok"), "✓ ok");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("trailing \u{1b}[32"), "trailing ");
    }

    #[test]
    fn test_parse_glab_masked_token_is_none() {
        assert_eq!(
            parse("  ✓ Token found in operating system keyring: **************************"),
            None
        );
    }

    #[test]
    fn test_parse_glab_no_token_is_none() {
        let stderr = "gitlab.com\n  ! No token found (checked config file, keyring, and environment variables).\n";
        assert_eq!(parse(stderr), None);
    }

    #[test]
    fn test_parse_glab_ignores_other_token_lines() {
        let stderr = "! Token is from environment variable GITLAB_TOKEN. This takes precedence.\n\
                      ✓ Token found in environment variable GITLAB_TOKEN: glpat-abc123\n";
        assert_eq!(parse(stderr).as_deref(), Some("glpat-abc123"));
    }

    #[test]
    fn test_parse_glab_empty_token_is_none() {
        assert_eq!(parse("✓ Token found in operating system keyring: "), None);
        assert_eq!(parse("Token found"), None);
        assert_eq!(parse("Token foundry: x"), None);
    }

    #[test]
    fn test_glab_status_args_without_host() {
        assert_eq!(glab_status_args(None), vec!["auth", "status"]);
    }

    #[test]
    fn test_glab_status_args_with_host() {
        assert_eq!(
            glab_status_args(Some("gitlab.example.com")),
            vec!["auth", "status", "--hostname", "gitlab.example.com"]
        );
    }

    #[test]
    fn test_missing_gitlab_token_error_names_host_and_env() {
        let err = missing_token_error(
            ForgeKind::GitLab,
            Some("gitlab.example.com"),
            Some("WORK_GITLAB_TOKEN"),
        )
        .to_string();
        assert!(
            err.contains("glab auth login --hostname gitlab.example.com"),
            "{err}"
        );
        assert!(err.contains("WORK_GITLAB_TOKEN"), "{err}");
    }

    #[test]
    fn test_glab_login_hint_names_host() {
        assert_eq!(glab_login_hint(None), "glab auth login");
        assert_eq!(
            glab_login_hint(Some("gitlab.example.com")),
            "glab auth login --hostname gitlab.example.com"
        );
    }

    /// A stub `glab` modelled on the real one:
    /// - logged in to two instances, it reports both unless `--hostname`
    ///   narrows it, listing gitlab.com first so taking the first token
    ///   without `--hostname` would pick the wrong instance's;
    /// - its stored OAuth token is expired, and `--show-token` prints the
    ///   stored value before the status call refreshes it, so only a run
    ///   that follows an earlier status call sees the fresh token.
    ///
    /// It is run as `sh <script>` rather than exec'd. Writing an executable
    /// and exec'ing it at once fails intermittently on Linux with ETXTBSY,
    /// because a process forked by another test thread can inherit the
    /// still-open write handle.
    #[cfg(unix)]
    fn stub_glab(dir: &std::path::Path) -> impl Fn() -> Command {
        let script = dir.join("glab.sh");
        let state = dir.join("refreshed");
        std::fs::write(
            &script,
            format!(
                r#"if [ -e "{state}" ]; then age=fresh; else age=stale; fi
touch "{state}"
case "$*" in
  "auth status --hostname gitlab.example.com --show-token")
    echo "gitlab.example.com" >&2
    echo "  ✓ Token found in operating system keyring: tok-example-$age" >&2 ;;
  "auth status --show-token")
    echo "gitlab.com" >&2
    echo "  ✓ Token found in operating system keyring: tok-gitlab-com-$age" >&2
    echo "gitlab.example.com" >&2
    echo "  ✓ Token found in operating system keyring: tok-example-$age" >&2 ;;
  "auth status --hostname gitlab.example.com"|"auth status")
    echo "  ✓ Logged in" >&2 ;;
  *)
    echo "unexpected args: $*" >&2
    exit 1 ;;
esac
"#,
                state = state.display()
            ),
        )
        .unwrap();
        move || {
            let mut cmd = Command::new("sh");
            cmd.arg(&script);
            cmd
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_glab_auth_token_passes_hostname() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            glab_auth_token_with(stub_glab(dir.path()), Some("gitlab.example.com")).as_deref(),
            Some("tok-example-fresh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_glab_auth_token_without_host_takes_first() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            glab_auth_token_with(stub_glab(dir.path()), None).as_deref(),
            Some("tok-gitlab-com-fresh")
        );
    }

    #[test]
    fn test_glab_auth_token_missing_binary_is_none() {
        assert_eq!(
            glab_auth_token_with(
                || Command::new("/nonexistent/jjpr-test/glab"),
                Some("gitlab.com")
            ),
            None
        );
    }
}
