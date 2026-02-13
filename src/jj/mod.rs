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
    fn get_branch_changes(&self, to_commit_id: &str) -> Result<Vec<LogEntry>>;
    fn get_git_remotes(&self) -> Result<Vec<GitRemote>>;
    fn get_default_branch(&self) -> Result<String>;
    fn push_bookmark(&self, name: &str, remote: &str) -> Result<()>;
}
