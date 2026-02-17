pub mod comment;
pub mod gh_cli;
pub mod remote;
pub mod types;

pub use gh_cli::GhCli;
pub use types::*;

use std::collections::HashMap;

use anyhow::Result;

/// Build a map of branch name → PR, filtering out PRs from forks.
pub fn build_pr_map(prs: Vec<PullRequest>, owner: &str) -> HashMap<String, PullRequest> {
    let owner_prefix = format!("{owner}:");
    prs.into_iter()
        .filter(|pr| pr.head.label.starts_with(&owner_prefix) || pr.head.label.is_empty())
        .map(|pr| (pr.head.ref_name.clone(), pr))
        .collect()
}

/// Trait abstracting GitHub operations for testability.
pub trait GitHub: Send + Sync {
    fn list_open_prs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<PullRequest>>;

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
        number: u64,
    ) -> Result<Vec<IssueComment>>;

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
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

    fn mark_pr_ready(
        &self,
        owner: &str,
        repo: &str,
        pr_node_id: &str,
    ) -> Result<()>;

    fn get_authenticated_user(&self) -> Result<String>;

    fn find_merged_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>>;

    fn merge_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        method: MergeMethod,
    ) -> Result<()>;

    fn get_pr_checks_status(
        &self,
        owner: &str,
        repo: &str,
        head_ref: &str,
    ) -> Result<ChecksStatus>;

    fn get_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<ReviewSummary>;

    fn get_pr_mergeability(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrMergeability>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pr(ref_name: &str, label: &str) -> PullRequest {
        PullRequest {
            number: 1,
            html_url: String::new(),
            title: String::new(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new() },
            head: PullRequestRef { ref_name: ref_name.to_string(), label: label.to_string() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
        }
    }

    #[test]
    fn test_build_pr_map_filters_forks() {
        let prs = vec![
            make_pr("feature", "owner:feature"),
            make_pr("other", "someone-else:other"),
        ];
        let map = build_pr_map(prs, "owner");
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("feature"));
    }

    #[test]
    fn test_build_pr_map_accepts_empty_label() {
        let prs = vec![make_pr("feature", "")];
        let map = build_pr_map(prs, "owner");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_build_pr_map_empty_input() {
        let map = build_pr_map(vec![], "owner");
        assert!(map.is_empty());
    }
}
