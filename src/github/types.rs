use serde::Deserialize;

/// Repository owner and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub owner: String,
    pub repo: String,
}

/// A GitHub pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
    pub title: String,
    pub body: Option<String>,
    pub base: PullRequestRef,
    pub head: PullRequestRef,
}

/// A ref (base or head) on a pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
}

/// A comment on a GitHub issue/PR.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub body: Option<String>,
}
