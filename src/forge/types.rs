use serde::{Deserialize, Serialize};

/// Repository owner and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub owner: String,
    pub repo: String,
}

/// A pull request / merge request from any supported forge.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub html_url: String,
    pub title: String,
    pub body: Option<String>,
    pub base: PullRequestRef,
    pub head: PullRequestRef,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub merged_at: Option<String>,
}

/// A ref (base or head) on a pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(default)]
    pub label: String,
}

/// A comment on an issue or pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub body: Option<String>,
}

/// Merge method for a pull request.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    #[default]
    Squash,
    Merge,
    Rebase,
}

impl std::fmt::Display for MergeMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Squash => write!(f, "squash"),
            Self::Merge => write!(f, "merge"),
            Self::Rebase => write!(f, "rebase"),
        }
    }
}

/// Status of CI checks on a PR's head ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksStatus {
    /// All checks passed.
    Pass,
    /// Some checks are still running.
    Pending,
    /// One or more checks failed.
    Fail,
    /// No checks configured on this repo/branch.
    None,
}

/// Review summary for a PR.
#[derive(Debug, Clone)]
pub struct ReviewSummary {
    pub approved_count: u32,
    pub changes_requested: bool,
}

/// Lightweight PR state for verifying merge outcomes.
#[derive(Debug, Clone)]
pub struct PrState {
    pub merged: bool,
    pub state: String,
}

/// Mergeability status from the single-PR endpoint.
#[derive(Debug, Clone)]
pub struct PrMergeability {
    /// `None` means the forge is still computing.
    pub mergeable: Option<bool>,
    /// "clean", "dirty", "blocked", "behind", "unknown", etc.
    pub mergeable_state: String,
}
