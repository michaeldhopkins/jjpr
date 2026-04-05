use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::forge::ForgeKind;
use crate::forge::types::MergeMethod;

/// How to reconcile the remaining stack after merging a PR.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReconcileStrategy {
    /// Create merge commits incorporating the new base.
    Merge,
    /// Rebase downstream commits onto the new base.
    #[default]
    Rebase,
}

/// Where to display stack navigation on PRs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StackNavMode {
    /// Stack navigation as a PR comment (default).
    #[default]
    Comment,
    /// Stack navigation embedded in the PR description/body.
    Description,
}

/// User configuration for jjpr.
///
/// Loaded from `~/.config/jjpr/config.toml` (global) and optionally merged
/// with `.jj/jjpr.toml` (repo-local). Repo-local fields override global.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub merge_method: MergeMethod,
    pub required_approvals: u32,
    pub require_ci_pass: bool,

    /// Explicit forge type override. When set, auto-detection is skipped.
    pub forge: Option<ForgeKind>,

    /// Name of the environment variable holding the forge API token.
    /// Falls back to the forge's default (GITHUB_TOKEN, GITLAB_TOKEN, FORGEJO_TOKEN).
    pub forge_token_env: Option<String>,

    /// How to sync the remaining stack after merging a PR.
    /// "rebase" (default): rebase onto new base.
    /// "merge": create merge commits incorporating the new base.
    pub reconcile_strategy: ReconcileStrategy,

    /// Where to display stack navigation: "comment" (default) or "description".
    /// "comment" posts a separate comment on each PR.
    /// "description" embeds the stack nav in the PR body.
    pub stack_nav: StackNavMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            merge_method: MergeMethod::Squash,
            required_approvals: 1,
            require_ci_pass: true,
            forge: None,
            forge_token_env: None,
            reconcile_strategy: ReconcileStrategy::Rebase,
            stack_nav: StackNavMode::Comment,
        }
    }
}

/// Returns the global config file path: `$XDG_CONFIG_HOME/jjpr/config.toml`
/// or `$HOME/.config/jjpr/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("jjpr").join("config.toml"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".config").join("jjpr").join("config.toml"))
}

/// Returns the repo-local config file path: `{repo_root}/.jj/jjpr.toml`.
pub fn repo_config_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".jj").join("jjpr.toml")
}

/// Load global config, falling back to defaults if the file doesn't exist.
pub fn load_config() -> Result<Config> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    load_config_from(&path)
}

/// Load config from a specific path, falling back to defaults if the file doesn't exist.
pub fn load_config_from(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Load config, merging repo-local `.jj/jjpr.toml` over global config.
/// Repo-local fields override global fields (field by field).
pub fn load_config_with_repo(repo_root: Option<&Path>) -> Result<Config> {
    let global_table = load_toml_table(config_path().as_deref())?;

    let repo_table = if let Some(root) = repo_root {
        load_toml_table(Some(&repo_config_path(root)))?
    } else {
        toml::map::Map::new()
    };

    let mut merged = global_table;
    for (key, value) in repo_table {
        merged.insert(key, value);
    }

    merged
        .try_into()
        .context("failed to parse merged configuration")
}

/// Load a TOML file as a key-value table. Returns an empty table if the file
/// doesn't exist.
fn load_toml_table(path: Option<&Path>) -> Result<toml::map::Map<String, toml::Value>> {
    let Some(path) = path else {
        return Ok(toml::map::Map::new());
    };
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let value: toml::Value = contents
                .parse()
                .with_context(|| format!("failed to parse {}", path.display()))?;
            match value {
                toml::Value::Table(table) => Ok(table),
                _ => anyhow::bail!("{} is not a TOML table", path.display()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::map::Map::new()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Write the default global config file, creating parent directories as needed.
/// Returns the path written to. Refuses to overwrite an existing file.
pub fn write_default_config() -> Result<PathBuf> {
    let path = config_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory (HOME not set)"))?;
    write_config_to(&path, DEFAULT_GLOBAL_CONFIG)?;
    Ok(path)
}

/// Write the repo-local config file at `.jj/jjpr.toml`.
/// Refuses to overwrite an existing file.
pub fn write_repo_config(repo_root: &Path) -> Result<PathBuf> {
    let path = repo_config_path(repo_root);
    write_config_to(&path, DEFAULT_REPO_CONFIG)?;
    Ok(path)
}

/// Write config content to a specific path. Refuses to overwrite an existing file.
pub fn write_config_to(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        anyhow::bail!("config file already exists at {}", path.display());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    std::fs::write(path, content)
        .with_context(|| format!("failed to write {}", path.display()))
}

const DEFAULT_GLOBAL_CONFIG: &str = r#"# jjpr configuration
# See: https://github.com/michaeldhopkins/jjpr

# Merge method: "squash", "merge", or "rebase"
merge_method = "squash"

# Number of approving reviews required before merging
required_approvals = 1

# Whether CI checks must pass before merging
require_ci_pass = true

# How to sync the remaining stack after merging a PR.
# "rebase" (default): rebases downstream commits onto the new base.
# "merge": creates merge commits on downstream branches.
reconcile_strategy = "rebase"

# Where to show stack navigation: "comment" (default) or "description".
# "comment" posts a separate comment on each PR.
# "description" embeds it in the PR body (more visible to reviewers).
stack_nav = "comment"
"#;

const DEFAULT_REPO_CONFIG: &str = r#"# jjpr repo-local configuration
# This file is gitignored via .jj/
# Repo-local settings override global settings (~/.config/jjpr/config.toml).
# See: https://github.com/michaeldhopkins/jjpr

# Forge type: "github", "gitlab", or "forgejo"
# Uncomment to override auto-detection (useful for self-hosted instances).
# forge = "forgejo"

# Environment variable name containing the forge API token.
# Falls back to the forge's default (GITHUB_TOKEN, GITLAB_TOKEN, FORGEJO_TOKEN).
# forge_token_env = "FORGEJO_TOKEN"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let config = Config::default();
        assert_eq!(config.merge_method, MergeMethod::Squash);
        assert_eq!(config.required_approvals, 1);
        assert!(config.require_ci_pass);
        assert!(config.forge.is_none());
        assert!(config.forge_token_env.is_none());
        assert_eq!(config.reconcile_strategy, ReconcileStrategy::Rebase);
    }

    #[test]
    fn test_parse_full_config() {
        let toml_str = r#"
merge_method = "rebase"
required_approvals = 2
require_ci_pass = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.merge_method, MergeMethod::Rebase);
        assert_eq!(config.required_approvals, 2);
        assert!(!config.require_ci_pass);
    }

    #[test]
    fn test_parse_partial_config() {
        let toml_str = r#"
merge_method = "merge"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.merge_method, MergeMethod::Merge);
        assert_eq!(config.required_approvals, 1);
        assert!(config.require_ci_pass);
    }

    #[test]
    fn test_parse_empty_config() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.merge_method, MergeMethod::Squash);
        assert_eq!(config.required_approvals, 1);
        assert!(config.require_ci_pass);
    }

    #[test]
    fn test_parse_invalid_toml() {
        let result: Result<Config, _> = toml::from_str("merge_method = [invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_merge_method() {
        let result: Result<Config, _> = toml::from_str(r#"merge_method = "yolo""#);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_forge_field() {
        let toml_str = r#"forge = "forgejo""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.forge, Some(ForgeKind::Forgejo));
    }

    #[test]
    fn test_parse_forge_github() {
        let toml_str = r#"forge = "github""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.forge, Some(ForgeKind::GitHub));
    }

    #[test]
    fn test_parse_forge_gitlab() {
        let toml_str = r#"forge = "gitlab""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.forge, Some(ForgeKind::GitLab));
    }

    #[test]
    fn test_parse_invalid_forge() {
        let result: Result<Config, _> = toml::from_str(r#"forge = "bitbucket""#);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_reconcile_strategy_merge() {
        let config: Config = toml::from_str(r#"reconcile_strategy = "merge""#).unwrap();
        assert_eq!(config.reconcile_strategy, ReconcileStrategy::Merge);
    }

    #[test]
    fn test_parse_reconcile_strategy_rebase() {
        let config: Config = toml::from_str(r#"reconcile_strategy = "rebase""#).unwrap();
        assert_eq!(config.reconcile_strategy, ReconcileStrategy::Rebase);
    }

    #[test]
    fn test_parse_invalid_reconcile_strategy() {
        let result: Result<Config, _> = toml::from_str(r#"reconcile_strategy = "yolo""#);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_forge_token_env() {
        let toml_str = r#"forge_token_env = "MY_CUSTOM_TOKEN""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.forge_token_env.as_deref(), Some("MY_CUSTOM_TOKEN"));
    }

    #[test]
    fn test_existing_configs_still_parse() {
        let config: Config = toml::from_str(DEFAULT_GLOBAL_CONFIG).unwrap();
        assert_eq!(config.merge_method, MergeMethod::Squash);
        assert!(config.forge.is_none());
    }

    #[test]
    fn test_repo_config_parses() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.forge.is_none());
        assert!(config.forge_token_env.is_none());
    }

    #[test]
    fn test_repo_config_overrides_global() {
        let dir = tempfile::TempDir::new().unwrap();

        let global_path = dir.path().join("global.toml");
        std::fs::write(&global_path, r#"
merge_method = "rebase"
required_approvals = 2
"#).unwrap();

        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(repo_root.join(".jj")).unwrap();
        let repo_path = repo_root.join(".jj").join("jjpr.toml");
        std::fs::write(&repo_path, r#"
forge = "forgejo"
merge_method = "squash"
"#).unwrap();

        let global_table = load_toml_table(Some(&global_path)).unwrap();
        let repo_table = load_toml_table(Some(&repo_path)).unwrap();

        let mut merged = global_table;
        for (key, value) in repo_table {
            merged.insert(key, value);
        }

        let config: Config = merged.try_into().unwrap();
        assert_eq!(config.forge, Some(ForgeKind::Forgejo));
        assert_eq!(config.merge_method, MergeMethod::Squash); // repo overrode
        assert_eq!(config.required_approvals, 2); // kept from global
    }

    #[test]
    fn test_load_config_with_repo_no_repo() {
        let config = load_config_with_repo(None).unwrap();
        assert!(config.forge.is_none());
    }

    #[test]
    fn test_load_config_with_repo_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = load_config_with_repo(Some(dir.path())).unwrap();
        assert!(config.forge.is_none());
    }

    #[test]
    fn test_load_missing_file() {
        let config = load_config_from(Path::new("/tmp/jjpr-nonexistent/config.toml")).unwrap();
        assert_eq!(config.merge_method, MergeMethod::Squash);
    }

    #[test]
    fn test_load_valid_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, r#"merge_method = "rebase""#).unwrap();

        let config = load_config_from(&path).unwrap();
        assert_eq!(config.merge_method, MergeMethod::Rebase);
    }

    #[test]
    fn test_load_invalid_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();

        let err = load_config_from(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("failed to parse"),
            "error should mention parsing: {err:#}"
        );
    }

    #[test]
    fn test_write_default_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("jjpr").join("config.toml");

        write_config_to(&path, DEFAULT_GLOBAL_CONFIG).unwrap();
        assert!(path.exists());

        let config = load_config_from(&path).unwrap();
        assert_eq!(config.merge_method, MergeMethod::Squash);
        assert_eq!(config.required_approvals, 1);
        assert!(config.require_ci_pass);
    }

    #[test]
    fn test_write_default_config_refuses_overwrite() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("jjpr").join("config.toml");

        write_config_to(&path, DEFAULT_GLOBAL_CONFIG).unwrap();
        let err = write_config_to(&path, DEFAULT_GLOBAL_CONFIG).unwrap_err();
        assert!(
            format!("{err:#}").contains("already exists"),
            "should refuse to overwrite: {err:#}"
        );
    }

    #[test]
    fn test_write_repo_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(repo_root.join(".jj")).unwrap();

        let path = write_repo_config(&repo_root).unwrap();
        assert!(path.exists());

        let config = load_config_from(&path).unwrap();
        assert!(config.forge.is_none());
    }

    #[test]
    fn test_parse_stack_nav_comment() {
        let config: Config = toml::from_str(r#"stack_nav = "comment""#).unwrap();
        assert_eq!(config.stack_nav, StackNavMode::Comment);
    }

    #[test]
    fn test_parse_stack_nav_description() {
        let config: Config = toml::from_str(r#"stack_nav = "description""#).unwrap();
        assert_eq!(config.stack_nav, StackNavMode::Description);
    }

    #[test]
    fn test_parse_invalid_stack_nav() {
        let result: Result<Config, _> = toml::from_str(r#"stack_nav = "inline""#);
        assert!(result.is_err());
    }

    #[test]
    fn test_stack_nav_defaults_to_comment() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.stack_nav, StackNavMode::Comment);
    }

    #[test]
    fn test_config_path_falls_back_to_home() {
        let path = config_path();
        assert!(path.is_some(), "should resolve a config path");
        assert!(
            path.unwrap().to_str().unwrap().contains("jjpr/config.toml"),
            "path should end with jjpr/config.toml"
        );
    }
}
