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
    /// PR author's login (GitHub/Forgejo `user.login`, GitLab `author.username`).
    /// Empty when the forge omits it (e.g. a deleted account).
    #[serde(default, rename = "user", deserialize_with = "deserialize_user_login")]
    pub author: String,
}

/// Extract a single author login from a nested user object. GitHub/Forgejo
/// expose the PR author at `user.login` (the field is renamed from `user`);
/// a null or missing user yields an empty string.
fn deserialize_user_login<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .get("login")
        .or_else(|| value.get("username"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
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

/// A native pull-request stack (GitHub preview feature).
///
/// Populated from `GET /repos/{owner}/{repo}/stacks`. Read-only for now; jjpr
/// does not create or mutate native stacks. The `number` shares the repo's
/// issue/PR numbering space (a stack consumes a number), and `pull_requests`
/// is ordered bottom-to-top (index 0 targets `base`, each later PR targets the
/// previous one's head — GitHub requires a fully linear chain).
#[derive(Debug, Clone, Deserialize)]
pub struct Stack {
    pub number: u64,
    #[serde(default)]
    pub node_id: String,
    /// The stack's ultimate target. Only `ref` is set at the list level; the
    /// copy embedded on a PR also carries `sha`.
    pub base: PullRequestRef,
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub pull_requests: Vec<StackPr>,
}

/// One pull request as it appears inside a `Stack`. Leaner than a full
/// `PullRequest`: the list endpoint returns only these fields.
#[derive(Debug, Clone, Deserialize)]
pub struct StackPr {
    pub number: u64,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged_at: Option<String>,
    pub head: PullRequestRef,
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

/// Which PRs in the stack should receive reviewer requests.
///
/// `Bottom` (default): only the bottommost LIVE PR — i.e., the lowest
/// segment that hasn't been merged yet. As the stack drains via merges,
/// the next iteration's bottom is whichever PR is now lowest. This is
/// the natural workflow: ask for review where it's actually needed next.
///
/// `Leaf`: only the topmost PR.
///
/// `All`: every PR in the stack. Old jjpr default; kept for users who
/// rely on it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReviewerScope {
    #[default]
    Bottom,
    Leaf,
    All,
}

impl std::fmt::Display for ReviewerScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bottom => write!(f, "bottom"),
            Self::Leaf => write!(f, "leaf"),
            Self::All => write!(f, "all"),
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
    ///
    /// Advisory only, and not uniform across forges: Forgejo has no equivalent
    /// field and synthesizes one, as does GitHub's batch path. Branch on
    /// [`Self::mergeable`] instead.
    pub mergeable_state: String,
}

impl PullRequest {
    /// The ref this PR's CI checks hang off.
    ///
    /// Prefer the head sha: querying by branch name can return checks for a
    /// commit the branch has since moved past, which reads as a stale pass right
    /// after a push. Not every forge populates the sha on its PR list, so the
    /// branch name remains the fallback.
    pub fn checks_ref(&self) -> &str {
        if self.head.sha.is_empty() {
            &self.head.ref_name
        } else {
            &self.head.sha
        }
    }
}

/// Everything the status view needs about one pull request.
///
/// Each field is independently optional because a forge may answer some parts
/// and not others; a missing field means "unknown", which renders as nothing
/// rather than as a false negative.
#[derive(Debug, Clone, Default)]
pub struct PrStatusBundle {
    pub mergeability: Option<PrMergeability>,
    pub checks: Option<ChecksStatus>,
    pub reviews: Option<ReviewSummary>,
}
