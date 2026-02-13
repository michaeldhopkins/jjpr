pub mod comment;
pub mod gh_cli;
pub mod remote;
pub mod types;

pub use gh_cli::GhCli;
pub use types::*;

use anyhow::Result;

/// Trait abstracting GitHub operations for testability.
pub trait GitHub: Send + Sync {
    fn find_open_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>>;

    fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        draft: bool,
    ) -> Result<PullRequest>;

    fn update_pr_base(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        base: &str,
    ) -> Result<()>;

    fn request_reviewers(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        reviewers: &[String],
    ) -> Result<()>;

    fn list_comments(
        &self,
        owner: &str,
        repo: &str,
        issue: u64,
    ) -> Result<Vec<IssueComment>>;

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        issue: u64,
        body: &str,
    ) -> Result<IssueComment>;

    fn update_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<()>;

    fn update_pr_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()>;

    fn convert_pr_to_ready(
        &self,
        owner: &str,
        repo: &str,
        pr_node_id: &str,
    ) -> Result<()>;

    fn get_authenticated_user(&self) -> Result<String>;
}
