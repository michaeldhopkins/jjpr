use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::forge::types::MergeMethod;

/// User configuration for jjpr, loaded from `~/.config/jjpr/config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub merge_method: MergeMethod,
    pub required_approvals: u32,
    pub require_ci_pass: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            merge_method: MergeMethod::Squash,
            required_approvals: 1,
            require_ci_pass: true,
        }
    }
}

/// Returns the config file path: `$XDG_CONFIG_HOME/jjpr/config.toml`
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

/// Load config from disk, falling back to defaults if the file doesn't exist.
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

/// Write the default config file, creating parent directories as needed.
/// Returns the path written to. Refuses to overwrite an existing file.
pub fn write_default_config() -> Result<PathBuf> {
    let path = config_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory (HOME not set)"))?;
    write_default_config_to(&path)?;
    Ok(path)
}

/// Write the default config to a specific path. Refuses to overwrite an existing file.
pub fn write_default_config_to(path: &Path) -> Result<()> {
    if path.exists() {
        anyhow::bail!("config file already exists at {}", path.display());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    std::fs::write(path, DEFAULT_CONFIG_CONTENT)
        .with_context(|| format!("failed to write {}", path.display()))
}

const DEFAULT_CONFIG_CONTENT: &str = r#"# jjpr configuration
# See: https://github.com/michaeldhopkins/jjpr

# Merge method: "squash", "merge", or "rebase"
merge_method = "squash"

# Number of approving reviews required before merging
required_approvals = 1

# Whether CI checks must pass before merging
require_ci_pass = true
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

        write_default_config_to(&path).unwrap();
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

        write_default_config_to(&path).unwrap();
        let err = write_default_config_to(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("already exists"),
            "should refuse to overwrite: {err:#}"
        );
    }

    #[test]
    fn test_config_path_falls_back_to_home() {
        // config_path uses HOME if XDG_CONFIG_HOME is not set; we just verify it returns Some
        let path = config_path();
        assert!(path.is_some(), "should resolve a config path");
        assert!(
            path.unwrap().to_str().unwrap().contains("jjpr/config.toml"),
            "path should end with jjpr/config.toml"
        );
    }
}
