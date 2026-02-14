use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use super::templates::{self, BOOKMARK_TEMPLATE, LOG_TEMPLATE};
use super::types::{Bookmark, GitRemote, LogEntry};
use super::Jj;

/// Real jj implementation that shells out to the jj binary.
pub struct JjRunner {
    repo_path: PathBuf,
}

impl JjRunner {
    pub fn new(repo_path: PathBuf) -> Result<Self> {
        // Verify jj is available
        Command::new("jj")
            .arg("--version")
            .output()
            .context("jj not found. Install it: https://jj-vcs.github.io/jj/")?;

        // Verify path is a jj repo
        if !repo_path.join(".jj").is_dir() {
            anyhow::bail!("{} is not a jj repository", repo_path.display());
        }

        Ok(Self { repo_path })
    }

    fn run_jj(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("jj")
            .args(args)
            .current_dir(&self.repo_path)
            .output()
            .context("failed to run jj")?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("jj {} failed: {}", args.join(" "), stderr.trim())
        }
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }
}

impl Jj for JjRunner {
    fn git_fetch(&self) -> Result<()> {
        self.run_jj(&["git", "fetch", "--all-remotes"])?;
        Ok(())
    }

    fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
        let output = self.run_jj(&[
            "bookmark",
            "list",
            "--revisions",
            "mine() ~ trunk()",
            "--template",
            BOOKMARK_TEMPLATE,
        ])?;
        templates::parse_bookmark_output(&output)
    }

    fn get_changes_to_commit(&self, to_commit_id: &str) -> Result<Vec<LogEntry>> {
        let revset = format!(r#"trunk().."{to_commit_id}""#);

        let output = self.run_jj(&[
            "log",
            "--revisions",
            &revset,
            "--no-graph",
            "--template",
            LOG_TEMPLATE,
        ])?;
        templates::parse_log_output(&output)
    }

    fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
        let output = self.run_jj(&["git", "remote", "list"])?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, ' ');
                let name = parts.next()?.trim().to_string();
                let url = parts.next()?.trim().to_string();
                if name.is_empty() {
                    return None;
                }
                Some(GitRemote { name, url })
            })
            .collect())
    }

    fn get_default_branch(&self) -> Result<String> {
        // Look at remote bookmarks on trunk() to find the default branch name
        let template = r#"remote_bookmarks.map(|b| b.name()).join(",")"#;
        let output = self.run_jj(&[
            "log",
            "--revisions",
            "trunk()",
            "--no-graph",
            "--limit",
            "1",
            "--template",
            template,
        ])?;

        let bookmarks: Vec<&str> = output.trim().split(',').collect();
        const DEFAULT_BRANCH_CANDIDATES: &[&str] = &["main", "master", "trunk"];
        for candidate in DEFAULT_BRANCH_CANDIDATES {
            if bookmarks.iter().any(|b| b.trim() == *candidate) {
                return Ok(candidate.to_string());
            }
        }

        // Fall back to the first bookmark found, or "main"
        bookmarks
            .first()
            .filter(|b| !b.trim().is_empty())
            .map(|b| b.trim().to_string())
            .ok_or_else(|| anyhow::anyhow!("could not determine default branch"))
    }

    fn push_bookmark(&self, name: &str, remote: &str) -> Result<()> {
        self.run_jj(&[
            "git",
            "push",
            "--remote",
            remote,
            "--bookmark",
            name,
            "--allow-new",
        ])?;
        Ok(())
    }

    fn get_working_copy_commit_id(&self) -> Result<String> {
        let output = self.run_jj(&[
            "log", "-r", "@", "--no-graph", "--limit", "1",
            "--template", "commit_id",
        ])?;
        let id = output.trim().to_string();
        if id.is_empty() {
            anyhow::bail!("could not determine working copy commit");
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jj_available() -> bool {
        Command::new("jj")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn init_jj_repo(path: &Path) {
        Command::new("jj")
            .args(["git", "init"])
            .current_dir(path)
            .output()
            .expect("failed to init jj repo");
    }

    #[test]
    fn test_jj_runner_rejects_non_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let result = JjRunner::new(temp.path().to_path_buf());
        assert!(result.is_err());
    }

    #[test]
    fn test_get_git_remotes_empty() {
        if !jj_available() {
            return;
        }

        let temp = tempfile::TempDir::new().unwrap();
        init_jj_repo(temp.path());

        let runner = JjRunner::new(temp.path().to_path_buf()).unwrap();
        let remotes = runner.get_git_remotes().unwrap();
        assert!(remotes.is_empty());
    }

    #[test]
    fn test_get_my_bookmarks_empty_repo() {
        if !jj_available() {
            return;
        }

        let temp = tempfile::TempDir::new().unwrap();
        init_jj_repo(temp.path());

        let runner = JjRunner::new(temp.path().to_path_buf()).unwrap();
        let bookmarks = runner.get_my_bookmarks().unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_get_my_bookmarks_with_bookmark() {
        if !jj_available() {
            return;
        }

        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path();
        init_jj_repo(repo);

        std::fs::write(repo.join("file.txt"), "content\n").unwrap();
        Command::new("jj")
            .args(["commit", "-m", "initial"])
            .current_dir(repo)
            .output()
            .unwrap();
        Command::new("jj")
            .args(["bookmark", "set", "feature", "-r", "@-"])
            .current_dir(repo)
            .output()
            .unwrap();

        let runner = JjRunner::new(repo.to_path_buf()).unwrap();
        let bookmarks = runner.get_my_bookmarks().unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "feature");
    }

    #[test]
    fn test_repo_path() {
        if !jj_available() {
            return;
        }

        let temp = tempfile::TempDir::new().unwrap();
        init_jj_repo(temp.path());

        let runner = JjRunner::new(temp.path().to_path_buf()).unwrap();
        assert_eq!(runner.repo_path(), temp.path());
    }
}
