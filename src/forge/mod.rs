pub mod comment;
pub mod forgejo;
pub mod github;
pub mod gitlab;
pub mod http;
pub mod remote;
pub mod token;
pub mod types;

pub use forgejo::ForgejoForge;
pub use github::GitHubForge;
pub use gitlab::GitLabForge;
pub use http::{AuthScheme, ForgeClient, PaginationStyle};
pub use types::*;

use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;

/// Which forge a remote points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    GitHub,
    GitLab,
    Forgejo,
}

impl std::fmt::Display for ForgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitHub => write!(f, "GitHub"),
            Self::GitLab => write!(f, "GitLab"),
            Self::Forgejo => write!(f, "Forgejo"),
        }
    }
}

impl ForgeKind {
    /// "pull request" or "merge request"
    pub fn request_noun(&self) -> &'static str {
        match self {
            Self::GitHub | Self::Forgejo => "pull request",
            Self::GitLab => "merge request",
        }
    }

    /// "PR" or "MR"
    pub fn request_abbreviation(&self) -> &'static str {
        match self {
            Self::GitHub | Self::Forgejo => "PR",
            Self::GitLab => "MR",
        }
    }

    /// "#5" or "!5"
    pub fn format_ref(&self, number: u64) -> String {
        match self {
            Self::GitHub | Self::Forgejo => format!("#{number}"),
            Self::GitLab => format!("!{number}"),
        }
    }

    /// CLI name for help messages
    pub fn cli_name(&self) -> &'static str {
        match self {
            Self::GitHub => "gh",
            Self::GitLab => "glab",
            Self::Forgejo => "tea",
        }
    }

    /// Token environment variable name
    pub fn token_env_var(&self) -> &'static str {
        match self {
            Self::GitHub => "GITHUB_TOKEN",
            Self::GitLab => "GITLAB_TOKEN",
            Self::Forgejo => "FORGEJO_TOKEN",
        }
    }
}

/// Build a map of branch name → PR, filtering out PRs from forks.
///
/// Fork detection: GitHub/Forgejo use "owner:branch" labels for fork PRs.
/// Same-repo PRs have either "owner:branch" (matching our owner), just the branch
/// name (Codeberg), or an empty label (GitLab). We filter out only PRs whose label
/// contains ":" with a *different* owner prefix.
pub fn build_pr_map(prs: Vec<PullRequest>, owner: &str) -> HashMap<String, PullRequest> {
    let owner_prefix = format!("{owner}:");
    prs.into_iter()
        .filter(|pr| {
            pr.head.label.is_empty()
                || !pr.head.label.contains(':')
                || pr.head.label.starts_with(&owner_prefix)
        })
        .map(|pr| (pr.head.ref_name.clone(), pr))
        .collect()
}

/// Trait abstracting forge operations (GitHub, GitLab, Forgejo) for testability.
pub trait Forge: Send + Sync {
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
        number: u64,
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
    fn test_build_pr_map_accepts_label_without_owner_prefix() {
        // Codeberg/Forgejo returns just the branch name as label
        let prs = vec![make_pr("feature", "feature")];
        let map = build_pr_map(prs, "owner");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_build_pr_map_empty_input() {
        let map = build_pr_map(vec![], "owner");
        assert!(map.is_empty());
    }

    #[test]
    fn test_forge_kind_vocabulary() {
        assert_eq!(ForgeKind::GitHub.request_abbreviation(), "PR");
        assert_eq!(ForgeKind::GitLab.request_abbreviation(), "MR");
        assert_eq!(ForgeKind::Forgejo.request_abbreviation(), "PR");

        assert_eq!(ForgeKind::GitHub.format_ref(5), "#5");
        assert_eq!(ForgeKind::GitLab.format_ref(5), "!5");
        assert_eq!(ForgeKind::Forgejo.format_ref(5), "#5");

        assert_eq!(ForgeKind::GitHub.request_noun(), "pull request");
        assert_eq!(ForgeKind::GitLab.request_noun(), "merge request");

        assert_eq!(ForgeKind::GitHub.to_string(), "GitHub");
        assert_eq!(ForgeKind::GitLab.to_string(), "GitLab");
        assert_eq!(ForgeKind::Forgejo.to_string(), "Forgejo");
    }
}
