use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

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
    #[serde(default, deserialize_with = "deserialize_reviewer_logins")]
    pub requested_reviewers: Vec<String>,
}

/// Deserialize an array of user objects into a Vec of login/username strings.
/// Handles GitHub/Forgejo format (`[{"login": "alice"}, ...]`) and
/// GitLab format (`[{"username": "alice"}, ...]`).
fn deserialize_reviewer_logins<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ReviewerVisitor;

    impl<'de> Visitor<'de> for ReviewerVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an array of user objects with login or username fields, or null")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Vec<String>, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut logins = Vec::new();
            while let Some(obj) = seq.next_element::<serde_json::Value>()? {
                if let Some(login) = obj
                    .get("login")
                    .or_else(|| obj.get("username"))
                    .and_then(|v| v.as_str())
                {
                    logins.push(login.to_string());
                }
            }
            Ok(logins)
        }

        fn visit_none<E>(self) -> Result<Vec<String>, E>
        where
            E: de::Error,
        {
            Ok(Vec::new())
        }

        fn visit_unit<E>(self) -> Result<Vec<String>, E>
        where
            E: de::Error,
        {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(ReviewerVisitor)
}

/// A ref (base or head) on a pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub sha: String,
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
