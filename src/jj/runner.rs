use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use vcs_runner::{
    is_transient_error, jj_available, jj_current_operation_id, jj_divergent_change_ids,
    jj_op_restore, run_jj_utf8, run_jj_utf8_ignore_wc, run_jj_utf8_with_retry,
};

use super::Jj;
use super::templates::{self, BOOKMARK_TEMPLATE, LOG_TEMPLATE};
use super::types::{Bookmark, GitRemote, LogEntry};
use super::version;

/// Real jj implementation that shells out to the jj binary.
pub struct JjRunner {
    repo_path: PathBuf,
    /// The revset matching commits that are "yours" for ownership-scoped
    /// discovery. Defaults to jj's built-in `mine()` (single local email);
    /// [`JjRunner::set_identity`] widens it to all of your identities.
    owned_revset: String,
    /// Stale bookmarks already warned about. `watch` lists bookmarks on
    /// every poll with the same runner, and repeating the same warning
    /// each time buried the output that mattered.
    warned_stale: Mutex<HashSet<String>>,
    /// The forge's remote, when known: the one remote a fetch must reach.
    /// Unset, a fetch takes every remote in one call and any failure is fatal.
    fetch_remote: Option<String>,
}

impl JjRunner {
    pub fn new(repo_path: PathBuf) -> Result<Self> {
        if !jj_available() {
            anyhow::bail!("jj not found. Install it: https://jj-vcs.github.io/jj/");
        }

        if !repo_path.join(".jj").is_dir() {
            anyhow::bail!("{} is not a jj repository", repo_path.display());
        }

        Ok(Self {
            repo_path,
            owned_revset: "mine()".to_string(),
            warned_stale: Mutex::new(HashSet::new()),
            fetch_remote: None,
        })
    }

    /// Make `remote` the one a fetch requires (issue #11). Other remotes are
    /// still fetched, since trunk can live on one, but a failure on them is a
    /// warning: a mirror the network cannot reach must not stop a submit.
    pub fn set_fetch_remote(&mut self, remote: Option<String>) {
        self.fetch_remote = remote;
    }

    fn fetch(&self, args: &[&str]) -> Result<()> {
        // Fetch is pure-read into the git backend, so retrying on a transient
        // error (".lock", or "stale", which can follow a partial commit) is
        // safe here, unlike the mutating ops, which use plain `run_jj`.
        let args = [&["--ignore-working-copy", "git", "fetch"], args].concat();
        run_jj_utf8_with_retry(&self.repo_path, &args, is_transient_error)?;
        Ok(())
    }

    /// Warn once per stale bookmark for the life of this runner. Returns
    /// the names warned about on this call, so a repeat is observable.
    ///
    /// The hint is `jj bookmark forget` on its own: it touches one local
    /// bookmark and pushes nothing. `jj git push --deleted` would push
    /// every pending deletion in the repo, including ones the user never
    /// meant to publish.
    fn warn_stale_bookmarks(&self, warnings: Vec<String>) -> Vec<String> {
        let mut warned = self.warned_stale.lock().expect("poisoned");
        let mut fresh = Vec::new();
        for name in warnings {
            if !warned.insert(name.clone()) {
                continue;
            }
            eprintln!(
                "  Warning: skipping '{name}' (points to a missing or conflicted commit, typically after a squash merge on the forge)"
            );
            eprintln!("    To clean up the stale local bookmark:");
            eprintln!("      jj bookmark forget {name}");
            fresh.push(name);
        }
        fresh
    }

    /// Widen ownership discovery to every identity in `identity` (multiple
    /// commit emails across machines). Call once, after resolving the forge, and
    /// before discovery. Left unset, discovery uses `mine()` — the prior
    /// single-local-email behavior.
    pub fn set_identity(&mut self, identity: &crate::identity::Identity) {
        self.owned_revset = identity.owned_revset();
    }

    /// Run jj **working-copy-agnostically** — never snapshots or moves the
    /// user's checkout. This is jjpr's default: as a background actor it operates
    /// on committed, bookmarked state, so it must not perturb a live working
    /// copy. Returns lossy-decoded stdout, trimmed.
    fn run_jj(&self, args: &[&str]) -> Result<String> {
        Ok(run_jj_utf8_ignore_wc(&self.repo_path, args)?)
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
        let Some(required) = self.fetch_remote.as_deref() else {
            return self.fetch(&["--all-remotes"]);
        };
        self.fetch(&["--remote", required])?;
        for remote in self.get_git_remotes()? {
            if remote.name != required
                && let Err(e) = self.fetch(&["--remote", &remote.name])
            {
                eprintln!(
                    "  Warning: could not fetch remote '{}'; continuing without it.\n    {e}",
                    remote.name
                );
            }
        }
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

    fn get_status_bookmarks(&self, all_owned_stacks: bool) -> Result<Vec<Bookmark>> {
        // Author-agnostic, so a coworker's branch you've stacked on (or are
        // sitting directly on) shows up in `status`. The bare working-copy view
        // needs only `@`'s ancestry; `::mine()` (ancestors of every commit you
        // authored) is a large, wasted closure there, so add it only when we
        // must also find your other stacks by name (positional / --all).
        let revset = if all_owned_stacks {
            format!("::(@ | ({})) ~ trunk()", self.owned_revset)
        } else {
            "::@ ~ trunk()".to_string()
        };
        let output = self.run_jj(&[
            "bookmark",
            "list",
            "--revisions",
            &revset,
            "--template",
            BOOKMARK_TEMPLATE,
        ])?;
        let (bookmarks, warnings) = templates::parse_bookmark_output(&output)?;
        self.warn_stale_bookmarks(warnings);
        Ok(bookmarks)
    }

    fn get_user_email(&self) -> Result<String> {
        // Unset user.email → `config get` errors; treat as empty rather than
        // failing the whole command.
        Ok(self
            .run_jj(&["config", "get", "user.email"])
            .unwrap_or_default())
    }

    fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
        let revset = format!("({}) ~ trunk()", self.owned_revset);
        let output = self.run_jj(&[
            "bookmark",
            "list",
            "--revisions",
            &revset,
            "--template",
            BOOKMARK_TEMPLATE,
        ])?;
        let (bookmarks, warnings) = templates::parse_bookmark_output(&output)?;
        self.warn_stale_bookmarks(warnings);
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
        Ok(templates::parse_remote_list(&output))
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

        let output = self.run_jj(&[
            "log",
            "--revisions",
            "trunk()",
            "--no-graph",
            "--limit",
            "1",
            "--template",
            templates::TRUNK_BOOKMARKS_TEMPLATE,
        ])?;

        templates::parse_default_branch(&output)
            .ok_or_else(|| anyhow::anyhow!("could not determine default branch"))
    }

    fn push_bookmark(&self, name: &str, remote: &str) -> Result<()> {
        // A never-pushed bookmark needs a setting on jj 0.36/0.37 (see version.rs).
        let mut args = version::push_new_bookmark_args(version::installed_jj_version()).to_vec();
        args.extend(["git", "push", "--remote", remote, "--bookmark", name]);
        self.run_jj(&args)?;
        Ok(())
    }

    fn get_working_copy_commit_id(&self) -> Result<String> {
        let output = self.run_jj(&[
            "log",
            "-r",
            "@",
            "--no-graph",
            "--limit",
            "1",
            "--template",
            "commit_id",
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

    fn rebase_onto_all(&self, source: &str, destinations: &[String]) -> Result<()> {
        let mut args = vec!["rebase", "-s", source];
        for destination in destinations {
            args.extend(["-d", destination.as_str()]);
        }
        self.run_stack_op(source, &args)?;
        Ok(())
    }

    fn abandon(&self, revset: &str) -> Result<()> {
        self.run_stack_op(revset, &["abandon", revset])?;
        Ok(())
    }

    fn rebase_onto_skipping_emptied(&self, source: &str, destination: &str) -> Result<()> {
        let args = ["rebase", "-s", source, "-d", destination, "--skip-emptied"];
        self.run_stack_op(source, &args)?;
        Ok(())
    }

    fn is_rooted_in(&self, root: &str, base: &str) -> Result<bool> {
        // Parents of `root` that are NOT ancestors of `base`. Empty ⇒ every
        // parent is already under `base`, so the subtree sits cleanly on it.
        // Working-copy-agnostic (via run_jj) so the check never snapshots edits.
        let revset = format!("parents({root}) ~ ::{base}");
        let out = self.run_jj(&["log", "-r", &revset, "--no-graph", "-T", r#""x""#])?;
        Ok(out.trim().is_empty())
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
        // `change_id()`, not the bare id: a bare change id is a symbol, and jj refuses
        // to resolve a divergent symbol ("Change ID <x> is divergent"), which is
        // exactly the case this exists to count. The `all:` prefix jjpr used before
        // did not help (it only lifted the one-revision limit) and jj 0.38 removed it.
        let revset = format!("change_id({change_id})");
        let output = self.run_jj(&[
            "log",
            "-r",
            &revset,
            "--no-graph",
            "-T",
            r#"commit_id ++ "\n""#,
        ])?;
        Ok(output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect())
    }

    fn is_conflicted(&self, revset: &str) -> Result<bool> {
        // "Does ANY commit in `revset` have a conflict", correct for a revset of
        // any size. Templating `if(conflict, ...)` per commit and comparing the
        // whole output to "true" is only right for a single commit: two commits
        // yield "falsetrue", which compares unequal and silently reports clean.
        // Intersecting with `conflicts()` and testing for emptiness sidesteps
        // the concatenation entirely.
        let output = self.run_jj(&[
            "log",
            "-r",
            &format!("conflicts() & ({revset})"),
            "--no-graph",
            "-T",
            r#""x""#,
        ])?;
        Ok(!output.trim().is_empty())
    }

    // Operation-log primitives for concurrent-modification recovery. `op restore`
    // and the current-op id delegate to vcs-runner; the recovery *policy* (gate on
    // divergence, restore only the mangling rebase) stays in jjpr (src/merge).

    fn current_operation_id(&self) -> Result<String> {
        Ok(jj_current_operation_id(&self.repo_path)?)
    }

    fn divergent_change_ids(&self) -> Result<Vec<String>> {
        // vcs-runner reads this working-copy-agnostically (it must stay
        // readable when a concurrent writer left the working copy stale — the
        // exact situation this signal detects) and dedups to distinct changes.
        // From 0.18 it also works on jj 0.36/0.37, which have no `divergent()`
        // revset: 0.15 spelled the query with it, so every call failed there.
        Ok(jj_divergent_change_ids(&self.repo_path)?)
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
    fn test_stale_bookmark_warning_prints_once_per_runner() {
        if !jj_available() {
            return;
        }

        let temp = tempfile::TempDir::new().unwrap();
        init_jj_repo(temp.path());
        let runner = JjRunner::new(temp.path().to_path_buf()).unwrap();

        let first = runner.warn_stale_bookmarks(vec!["stale".to_string(), "other".to_string()]);
        assert_eq!(first, vec!["stale", "other"]);

        // The same names on the next poll are silent; a new one still prints.
        let second = runner.warn_stale_bookmarks(vec!["stale".to_string(), "third".to_string()]);
        assert_eq!(second, vec!["third"]);
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

    /// A repo with a reachable `origin` (an empty bare git repo) and a
    /// `backup` remote whose path does not exist, so fetching it fails at once.
    fn repo_with_an_unreachable_remote(temp: &tempfile::TempDir) -> PathBuf {
        let origin = temp.path().join("origin.git");
        let status = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&origin)
            .status()
            .expect("git init --bare");
        assert!(status.success());
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_jj_repo(&repo);
        let missing = temp.path().join("no-such-remote.git");
        for (name, url) in [("origin", origin.as_path()), ("backup", missing.as_path())] {
            let out = Command::new("jj")
                .args(["git", "remote", "add", name])
                .arg(url)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        repo
    }

    /// Issue #11: an unreachable remote that is not the forge's must not abort
    /// the fetch. Fetching every remote at once is what used to.
    #[test]
    fn fetch_survives_an_unreachable_remote_that_is_not_required() {
        if !jj_available() {
            return;
        }
        let temp = tempfile::TempDir::new().unwrap();
        let repo = repo_with_an_unreachable_remote(&temp);
        let mut runner = JjRunner::new(repo).unwrap();

        assert!(
            runner.git_fetch().is_err(),
            "with no required remote every remote must still be fetched, as before"
        );

        runner.set_fetch_remote(Some("origin".to_string()));
        runner
            .git_fetch()
            .expect("the unreachable remote is not the one the command needs");
    }

    /// Remotes other than the required one are still fetched, since trunk can
    /// live on one of them.
    #[test]
    fn fetch_still_fetches_the_other_reachable_remotes() {
        if !jj_available() {
            return;
        }
        let temp = tempfile::TempDir::new().unwrap();
        let repo = repo_with_an_unreachable_remote(&temp);
        let mirror = temp.path().join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        for args in [
            &["git", "init", "--colocate"][..],
            &["commit", "-m", "on the mirror"],
            &["bookmark", "set", "landed", "-r", "@-"],
        ] {
            let out = Command::new("jj")
                .args(args)
                .current_dir(&mirror)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        let out = Command::new("jj")
            .args(["git", "remote", "add", "mirror"])
            .arg(&mirror)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let mut runner = JjRunner::new(repo).unwrap();
        runner.set_fetch_remote(Some("origin".to_string()));

        runner.git_fetch().unwrap();

        let fetched = runner
            .run_jj(&[
                "log",
                "-r",
                "landed@mirror",
                "--no-graph",
                "-T",
                "description",
            ])
            .expect("the mirror's bookmark was fetched");
        assert_eq!(fetched.trim(), "on the mirror");
    }

    /// The remote the command needs is still required: its failure is fatal.
    #[test]
    fn fetch_fails_when_the_required_remote_is_unreachable() {
        if !jj_available() {
            return;
        }
        let temp = tempfile::TempDir::new().unwrap();
        let repo = repo_with_an_unreachable_remote(&temp);
        let mut runner = JjRunner::new(repo).unwrap();

        runner.set_fetch_remote(Some("backup".to_string()));
        let err = runner.git_fetch().unwrap_err().to_string();
        assert!(err.contains("--remote backup"), "{err}");
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
