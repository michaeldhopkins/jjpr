use std::path::{Path, PathBuf};

use anyhow::Result;
use vcs_runner::{is_transient_error, jj_available, run_jj_utf8, run_jj_utf8_with_retry};

use super::templates::{self, BOOKMARK_TEMPLATE, LOG_TEMPLATE};
use super::types::{Bookmark, GitRemote, LogEntry};
use super::Jj;

/// Real jj implementation that shells out to the jj binary.
pub struct JjRunner {
    repo_path: PathBuf,
}

impl JjRunner {
    pub fn new(repo_path: PathBuf) -> Result<Self> {
        if !jj_available() {
            anyhow::bail!("jj not found. Install it: https://jj-vcs.github.io/jj/");
        }

        if !repo_path.join(".jj").is_dir() {
            anyhow::bail!("{} is not a jj repository", repo_path.display());
        }

        Ok(Self { repo_path })
    }

    /// Run jj and return lossy-decoded stdout with surrounding whitespace trimmed.
    fn run_jj(&self, args: &[&str]) -> Result<String> {
        Ok(run_jj_utf8(&self.repo_path, args)?)
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }
}

impl Jj for JjRunner {
    fn git_fetch(&self) -> Result<()> {
        // Only idempotent operations retry. `vcs_runner::is_transient_error`
        // matches both ".lock" (op didn't start — always safe) and "stale"
        // (working-copy staleness — op may have partially committed). Retrying
        // mutating ops like `jj new` or `jj rebase` on "stale" could create
        // duplicate commits, so those deliberately use `run_jj` (no retry).
        // Fetch is pure-read into the git backend; retrying is safe in both
        // cases.
        run_jj_utf8_with_retry(&self.repo_path, &["git", "fetch", "--all-remotes"], is_transient_error)?;
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
        if let Ok(alias) = self.run_jj(&["config", "get", r#"revset-aliases."trunk()""#]) {
            let alias = alias.trim();
            if let Some((name, _remote)) = alias.split_once('@')
                && !name.is_empty()
                && !name.contains(|c: char| c.is_whitespace() || c == '(' || c == '|')
            {
                return Ok(name.to_string());
            }
        }

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
        ])?;
        Ok(())
    }

    fn get_working_copy_commit_id(&self) -> Result<String> {
        let output = self.run_jj(&[
            "log", "-r", "@", "--no-graph", "--limit", "1",
            "--template", "commit_id",
        ])?;
        if output.is_empty() {
            anyhow::bail!("could not determine working copy commit");
        }
        Ok(output)
    }

    fn rebase_onto(&self, source: &str, destination: &str) -> Result<()> {
        self.run_jj(&["rebase", "-s", source, "-d", destination])?;
        Ok(())
    }

    fn merge_into(&self, bookmark: &str, dest: &str) -> Result<()> {
        let msg = format!("Merge {dest} into {bookmark}");
        self.run_jj(&["new", "--no-edit", "-m", &msg, bookmark, dest])?;
        let revset = format!("children({bookmark}) & children({dest})");
        self.run_jj(&["bookmark", "set", bookmark, "-r", &revset])?;
        Ok(())
    }

    fn resolve_change_id(&self, change_id: &str) -> Result<Vec<String>> {
        let revset = format!("all:{change_id}");
        let output = self.run_jj(&[
            "log", "-r", &revset, "--no-graph", "-T", r#"commit_id ++ "\n""#,
        ])?;
        Ok(output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    fn is_conflicted(&self, revset: &str) -> Result<bool> {
        let output = self.run_jj(&[
            "log", "-r", revset, "--no-graph", "-T", r#"if(conflict, "true", "false")"#,
        ])?;
        Ok(output.trim() == "true")
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

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
