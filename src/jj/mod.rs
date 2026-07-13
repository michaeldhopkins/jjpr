pub mod runner;
pub mod templates;
pub mod types;

pub use runner::JjRunner;
pub use types::*;

use anyhow::Result;

/// Trait abstracting jj operations for testability.
pub trait Jj: Send + Sync {
    fn git_fetch(&self) -> Result<()>;
    /// The configured local `user.email`, or empty if unset. Seeds the set of
    /// identities that count as you. Defaults to empty for stubs.
    fn get_user_email(&self) -> Result<String> {
        Ok(String::new())
    }
    fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>>;
    /// Bookmarks to display in `status`, regardless of author (unlike
    /// [`Jj::get_my_bookmarks`], which is author-scoped for the mutating
    /// commands). This surfaces a coworker's branch you've stacked on.
    ///
    /// `all_owned_stacks = false` (the bare working-copy view) discovers only
    /// the working copy's ancestry — cheap, and all `infer_target_stack` needs.
    /// `true` (positional or `--all`) also includes your own stacks elsewhere so
    /// they're findable by name, at the cost of a much larger ancestor closure.
    /// Defaults to `get_my_bookmarks` for stubs that don't distinguish.
    fn get_status_bookmarks(&self, all_owned_stacks: bool) -> Result<Vec<Bookmark>> {
        let _ = all_owned_stacks;
        self.get_my_bookmarks()
    }
    /// Get all changes between trunk and `to_commit_id`.
    fn get_changes_to_commit(&self, to_commit_id: &str) -> Result<Vec<LogEntry>>;
    fn get_git_remotes(&self) -> Result<Vec<GitRemote>>;
    fn get_default_branch(&self) -> Result<String>;
    fn push_bookmark(&self, name: &str, remote: &str) -> Result<()>;
    fn get_working_copy_commit_id(&self) -> Result<String>;
    /// Rebase the subtree rooted at `source` onto `destination`.
    /// Runs `jj rebase -s <source> -d <destination>`.
    fn rebase_onto(&self, source: &str, destination: &str) -> Result<()>;
    /// Create a merge commit combining `bookmark` and `dest`, then move the
    /// bookmark to it. Used for merge-based reconciliation (avoids force pushes).
    fn merge_into(&self, bookmark: &str, dest: &str) -> Result<()>;
    /// Resolve a change ID to its commit IDs. Returns >1 if divergent.
    fn resolve_change_id(&self, change_id: &str) -> Result<Vec<String>>;
    /// Check whether the commit at `revset` has unresolved conflicts.
    fn is_conflicted(&self, revset: &str) -> Result<bool>;

    /// Snapshot the working copy into `@` — capture the user's current edits.
    /// jjpr is otherwise working-copy-agnostic (it never snapshots incidentally);
    /// user-invoked commands like `submit` call this once so they act on the
    /// user's latest state, while the autonomous watch loop never does (it
    /// operates on committed, bookmarked state). Default is inert for stubs.
    fn snapshot(&self) -> Result<()> {
        Ok(())
    }

    // --- Operation-log awareness (concurrent-modification recovery) ---
    //
    // A second jj process mutating the same repo (e.g. `jjpr watch` racing a
    // foreground command) forks the operation log; jj then reconciles the two
    // heads. jj preserves both sides' commits, so the only corruption signature
    // is a divergent change — which `divergent_change_ids` reports directly. On
    // that signal the reconcile gates before its rebase, or rolls only the rebase
    // back to `current_operation_id`'s post-fetch value via `restore_operation`.
    // The defaults are inert so test stubs that don't care keep compiling.

    /// The id of the current (head) operation in the operation log. Captured
    /// after fetch as the point to `restore_operation` back to if a rebase mangles.
    fn current_operation_id(&self) -> Result<String> {
        Ok(String::new())
    }

    /// Change ids that are divergent (one change id on multiple visible commits).
    /// Non-empty is the corruption signal left by a concurrent op-log reconcile
    /// (or a rebase racing one) — the sole thing recovery gates on.
    fn divergent_change_ids(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Roll the repo back to `op_id` (`jj op restore`). The recovery primitive:
    /// undo only our own mangling rebase, back to the clean post-fetch op.
    fn restore_operation(&self, _op_id: &str) -> Result<()> {
        Ok(())
    }
}
