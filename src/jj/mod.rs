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
    /// Resolve a change ID to its commit IDs. Returns >1 if divergent.
    fn resolve_change_id(&self, change_id: &str) -> Result<Vec<String>>;
}
