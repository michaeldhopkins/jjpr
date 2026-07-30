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
    /// Membership in a GitHub native pull-request stack, `None` when the PR is
    /// unstacked or the forge has no such concept. GitHub embeds this in every
    /// PR payload, so reading it costs nothing beyond the request jjpr already
    /// makes — which is the point: merge must know before it tries.
    #[serde(default)]
    pub stack: Option<PrStackRef>,
}

/// A pull request's view of the native stack it belongs to.
///
/// This is the object GitHub embeds on a *pull request*, which is leaner than
/// the [`Stack`] resource returned by `/stacks`: it carries no `node_id`,
/// `created_at`, `url`, or member list, and — unlike `Stack` — it does carry
/// `position` and `size`. There is no web URL for a stack on either shape (see
/// `notes/forges/github-native-stacks.md`), so callers name it by number.
/// Every field is defaulted or optional on purpose. This is a preview-grade
/// schema, and it is embedded in the payload that carries *every* pull request
/// jjpr reads: a field that GitHub stops sending would otherwise fail the whole
/// `PullRequest` parse and take `list_open_prs` down with it, breaking jjpr on
/// the repo entirely. Degrading to a vague message is survivable; failing to
/// list PRs is not. Erring toward "still parses" also keeps the merge block
/// firing, which is the safe direction.
#[derive(Debug, Clone, Deserialize)]
pub struct PrStackRef {
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub id: u64,
    /// 1-based position from the bottom of the stack; 1 is the PR closest to
    /// trunk. Merging a stacked PR also merges everything below it, so this
    /// doubles as "how many PRs a merge here would land".
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub size: u32,
    /// The stack's ultimate target branch. Unused today; kept because it is the
    /// one field here that a native-merge path would need.
    #[serde(default, deserialize_with = "lenient_ref")]
    pub base: Option<PullRequestRef>,
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

/// Deserialize an optional nested ref, degrading a malformed one to `None`
/// rather than failing the whole payload.
///
/// `#[serde(default)]` on a field only defends the level it is written at. Every
/// field on the preview-grade types is defaulted, but `PullRequestRef::ref_name`
/// is required, so a `"base": {}` — an object that is present but has lost its
/// `ref` — made serde abandon the ENTIRE stack payload with
/// `missing field \`ref\``. Found by the `forge_payload` fuzz target on its first
/// run; the three hand-written partial-payload tests had all dropped whole keys
/// and so never produced a present-but-empty one.
///
/// Used only on the preview-grade types, where the schema can move under us
/// without a version bump. `PullRequest::base`/`head` stay strict on purpose: a
/// pull request with no base is a broken response from a stable API, and failing
/// loudly is the right answer there.
fn lenient_ref<'de, D>(deserializer: D) -> std::result::Result<Option<PullRequestRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<Option<PullRequestRef>>(value)
        .ok()
        .flatten())
}

/// A native pull-request stack (GitHub preview feature).
///
/// Populated from `GET /repos/{owner}/{repo}/stacks`. Read-only for now; jjpr
/// does not create or mutate native stacks. The `number` shares the repo's
/// issue/PR numbering space (a stack consumes a number), and `pull_requests`
/// is ordered bottom-to-top (index 0 targets `base`, each later PR targets the
/// previous one's head — GitHub requires a fully linear chain).
/// Like [`PrStackRef`], every field is defaulted: this is a preview-grade
/// schema, and a field GitHub stops sending must not turn into a hard parse
/// error in the middle of a merge.
#[derive(Debug, Clone, Deserialize)]
pub struct Stack {
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub node_id: String,
    /// The stack's ultimate target branch.
    #[serde(default, deserialize_with = "lenient_ref")]
    pub base: Option<PullRequestRef>,
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    /// Members ordered bottom to top: index 0 targets `base`, each later PR
    /// targets the previous one's head.
    ///
    /// **Merged members stay in the list.** After a partial merge the landed
    /// PRs remain here with `state: "closed"` and `merged_at` set, and the
    /// stack keeps its original size. Anything reasoning about "what would this
    /// merge land" must treat closed-with-`merged_at` as already done, and
    /// closed-*without* it as a closed-unmerged PR that makes the whole stack
    /// unmergeable.
    #[serde(default)]
    pub pull_requests: Vec<StackPr>,
}

/// One pull request as it appears inside a `Stack`. Leaner than a full
/// `PullRequest`: the list endpoint returns only these fields.
#[derive(Debug, Clone, Deserialize)]
pub struct StackPr {
    #[serde(default)]
    pub number: u64,
    /// `"open"` or `"closed"`. Pair with `merged_at` to tell a landed PR from a
    /// closed-unmerged one; the second poisons a stack merge.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default, deserialize_with = "lenient_ref")]
    pub head: Option<PullRequestRef>,
    /// Present on `GET /stacks/{n}`, absent from the list endpoint.
    #[serde(default, deserialize_with = "lenient_ref")]
    pub base: Option<PullRequestRef>,
}

impl StackPr {
    /// Already landed, so a stack merge will skip it rather than fail on it.
    pub fn is_merged(&self) -> bool {
        self.merged_at.is_some()
    }

    /// Closed without merging. GitHub refuses to merge a stack containing one
    /// of these, failing at poll time with "Pull request must be open and not
    /// in draft mode" and without naming which PR — so a caller wanting a
    /// useful message has to find it itself.
    pub fn is_closed_unmerged(&self) -> bool {
        self.state == "closed" && self.merged_at.is_none()
    }

    /// Would block a merge **that includes this PR**: closed-unmerged, or still
    /// a draft.
    ///
    /// Only meaningful for members a merge would actually land. GitHub's check
    /// is position-scoped, so a draft or closed PR *above* the target is
    /// irrelevant — prefer [`Stack::blocker_for`], which applies the scoping for
    /// you.
    pub fn would_block_merge(&self) -> bool {
        self.is_closed_unmerged() || self.draft
    }
}

impl Stack {
    /// The members a merge of `target` would land: everything from the bottom up
    /// to and including it. `None` when `target` is not a member.
    ///
    /// Merging a stacked PR lands every member below it, so this is the range
    /// any gate has to apply to, not just the target.
    pub fn members_landed_by(&self, target: u64) -> Option<&[StackPr]> {
        let idx = self.pull_requests.iter().position(|p| p.number == target)?;
        Some(&self.pull_requests[..=idx])
    }

    /// The first member that would stop a merge of `target`, if any.
    ///
    /// Verified against the live API: the scoping is real. With a draft at
    /// position 2 of 3, merging position 3 fails ("Pull request must be open and
    /// not in draft mode") while merging position 1 succeeds. So checking every
    /// member would report blockers that do not block.
    ///
    /// GitHub's own failure never names the offending PR, which is the whole
    /// reason to compute this locally.
    pub fn blocker_for(&self, target: u64) -> Option<&StackPr> {
        self.members_landed_by(target)?
            .iter()
            .find(|p| p.would_block_merge())
    }
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
