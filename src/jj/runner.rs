use std::path::{Path, PathBuf};

use anyhow::Result;
use vcs_runner::{
    is_transient_error, jj_available, jj_current_operation_id, jj_op_restore, run_jj_utf8,
    run_jj_utf8_with_retry,
};

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

    /// Run jj **working-copy-agnostically** — never snapshots or moves the
    /// user's checkout. This is jjpr's default: as a background actor it operates
    /// on committed, bookmarked state, so it must not perturb a live working
    /// copy. Returns lossy-decoded stdout, trimmed.
    fn run_jj(&self, args: &[&str]) -> Result<String> {
        let mut full = Vec::with_capacity(args.len() + 1);
        full.push("--ignore-working-copy");
        full.extend_from_slice(args);
        Ok(run_jj_utf8(&self.repo_path, &full)?)
    }

    /// Run jj **allowing** it to snapshot/update the working copy. Reserved for
    /// the few operations that intentionally touch `@`: an explicit snapshot, and
    /// the reconcile rebase/merge when the user is sitting on the affected commit.
    fn run_jj_touching_wc(&self, args: &[&str]) -> Result<String> {
        Ok(run_jj_utf8(&self.repo_path, args)?)
    }

    /// Run a stack-rewriting op (rebase/merge) working-copy-aware: touch the
    /// working copy only when `@` is inside `descendants(affected)` — then jj
    /// moves `@` and carries the user's edits along; otherwise stay agnostic so
    /// an unrelated checkout is never snapshotted or moved. Falls back to the
    /// safe (WC-updating) path if we can't tell.
    fn run_stack_op(&self, affected: &str, args: &[&str]) -> Result<String> {
        if self.working_copy_in_subtree(affected).unwrap_or(true) {
            self.run_jj_touching_wc(args)
        } else {
            self.run_jj(args)
        }
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    /// Whether the working-copy commit (`@`) is within `descendants(source)` —
    /// i.e. a rebase rooted at `source` would move it, so jj must update the
    /// working copy. Checked working-copy-agnostically (via `run_jj`) so the
    /// check itself never snapshots the user's uncommitted edits.
    fn working_copy_in_subtree(&self, source: &str) -> Result<bool> {
        let revset = format!("@ & descendants({source})");
        let out = self.run_jj(&["log", "-r", &revset, "--no-graph", "-T", r#""x""#])?;
        Ok(!out.trim().is_empty())
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
        run_jj_utf8_with_retry(
            &self.repo_path,
            &["--ignore-working-copy", "git", "fetch", "--all-remotes"],
            is_transient_error,
        )?;
        Ok(())
    }

    fn snapshot(&self) -> Result<()> {
        // Any command WITHOUT --ignore-working-copy snapshots the working copy;
        // `jj status` is a cheap, side-effect-free way to force it. The one place
        // jjpr snapshots on purpose — so a user-invoked command captures their
        // current edits, exactly as any interactive jj command would.
        self.run_jj_touching_wc(&["status"])?;
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
        let (bookmarks, warnings) = templates::parse_bookmark_output(&output)?;
        for name in warnings {
            eprintln!("  Warning: skipping '{name}' (points to a missing or conflicted commit, typically after a squash merge on the forge)");
            eprintln!("    To clean up the stale local bookmark:");
            eprintln!("      jj bookmark forget {name} && jj git push --deleted");
        }
        Ok(bookmarks)
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
        // Working-copy-aware: jj updates the checkout only if `@` is in the
        // rebased subtree (proven in tests/rebase_working_copy.rs — ignoring it
        // there strands the user with a stale working copy).
        self.run_stack_op(source, &["rebase", "-s", source, "-d", destination])?;
        Ok(())
    }

    fn merge_into(&self, bookmark: &str, dest: &str) -> Result<()> {
        let msg = format!("Merge {dest} into {bookmark}");
        // Same working-copy-awareness as the rebase: only snapshot when the
        // user's `@` is on the bookmark being advanced. `--no-edit` never moves
        // `@`, so this never strands; it just controls whether their WIP is
        // folded into the merge. The bookmark move never touches the WC.
        self.run_stack_op(bookmark, &["new", "--no-edit", "-m", &msg, bookmark, dest])?;
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

    // Operation-log primitives for concurrent-modification recovery. `op restore`
    // and the current-op id delegate to vcs-runner; the recovery *policy* (gate on
    // divergence, restore only the mangling rebase) stays in jjpr (src/merge).

    fn current_operation_id(&self) -> Result<String> {
        Ok(jj_current_operation_id(&self.repo_path)?)
    }

    fn divergent_change_ids(&self) -> Result<Vec<String>> {
        // Working-copy-agnostic (via run_jj): a concurrent writer can leave the
        // working copy stale, and this signal — which exists to detect exactly
        // that situation — must still be readable then. Going through vcs-runner
        // (no --ignore-working-copy) errors with "working copy is stale".
        let out = self.run_jj(&["log", "-r", "divergent()", "--no-graph", "-T", r#"change_id ++ "\n""#])?;
        let mut ids: Vec<String> = out.lines().filter(|l| !l.is_empty()).map(String::from).collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    fn restore_operation(&self, op_id: &str) -> Result<()> {
        Ok(jj_op_restore(&self.repo_path, op_id)?)
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
