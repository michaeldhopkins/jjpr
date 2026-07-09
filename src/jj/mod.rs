pub mod runner;
pub mod templates;
pub mod types;

pub use runner::JjRunner;
pub use types::*;

use anyhow::Result;

/// Trait abstracting jj operations for testability.
pub trait Jj: Send + Sync {
    fn git_fetch(&self) -> Result<()>;
    fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>>;
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

    // --- Operation-log awareness (concurrent-modification detection/recovery) ---
    //
    // A second jj process mutating the same working copy (e.g. `jjpr watch`
    // racing a foreground command) forks the operation log; jj then silently
    // "reconciles divergent operations" by merging the two heads, which can
    // corrupt the stack (collapse commits, drop files). These methods let us
    // record a known-good operation before mutating and roll back to it if a
    // reconcile happened. The defaults are inert so test stubs that don't care
    // keep compiling; only `JjRunner` (and divergence-specific test stubs)
    // override them.

    /// The id of the current (head) operation in the operation log. Record this
    /// before a batch of mutations to get a point to `restore_operation` back to.
    fn current_operation_id(&self) -> Result<String> {
        Ok(String::new())
    }

    /// First-line descriptions of the operations newer than `op_id` (exclusive),
    /// newest first. A "reconcile divergent operations" entry here means a
    /// concurrent writer forced a reconcile during our mutations.
    fn operation_descriptions_since(&self, _op_id: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Change ids that are divergent (one change id on multiple visible commits).
    /// Non-empty is the persistent tell left by a concurrent op-log reconcile.
    fn divergent_change_ids(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Roll the repo back to `op_id` (`jj op restore`). The recovery primitive:
    /// pick a known-good operation instead of shipping jj's mangled auto-merge.
    fn restore_operation(&self, _op_id: &str) -> Result<()> {
        Ok(())
    }

    /// Recent operations as `(id, first-line description)` pairs, newest first,
    /// up to `limit`. Used to recover a repo that is *already* divergent at the
    /// start of a reconcile: spot the "reconcile divergent operations" op and
    /// walk back to a clean operation to restore to.
    fn recent_operations(&self, _limit: usize) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    /// Whether divergent changes exist as of a specific past operation. Lets us
    /// find the most recent operation whose state predates the divergence.
    fn is_divergent_at_operation(&self, _op_id: &str) -> Result<bool> {
        Ok(false)
    }
}
