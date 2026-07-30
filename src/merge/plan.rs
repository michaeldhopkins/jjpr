use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::forge::types::{ChecksStatus, MergeMethod, PrStatusBundle, PullRequest, RepoInfo};
use crate::forge::{Forge, ForgeKind};
use crate::jj::types::NarrowedSegment;

/// Why a PR can't be merged right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockReason {
    NoPr,
    Draft,
    ChecksFailing,
    ChecksPending,
    InsufficientApprovals { have: u32, need: u32 },
    ChangesRequested,
    Conflicted,
    MergeabilityUnknown,
    /// Local repo state diverged from the forge during the previous
    /// reconcile: failed fetch, rebase, or push, or a divergent change
    /// ID. Continuing risks merging the next PR with a bloated diff
    /// because the local stack was never rebased onto the new base.
    /// Recovery is local: `jj git fetch && jj rebase ...`, then re-run.
    LocalSyncFailed,
    /// Forge-side reconcile failed: list_open_prs, update_pr_base, or
    /// stack-comment update returned an error. The forge state may be
    /// stale or incomplete, so we can't safely evaluate the next PR.
    /// Recovery is usually retry; persistent failures need network or
    /// permission investigation.
    ForgeReconcileFailed,
    /// A concurrent jj process reconciled the operation log during our
    /// reconcile. jj preserves both sides' work, so we paused before the
    /// mangling rebase (or rolled only the rebase back to the clean post-fetch
    /// op) rather than ship a mangled tree — no work is discarded. Transient:
    /// the next poll retries. Persistent means another jj/jjpr process is still
    /// running on this repo and needs to be paused.
    ConcurrentModification,
    /// The PR belongs to a GitHub native pull-request stack, which GitHub
    /// refuses to merge over the ordinary merge endpoint (`403`). Users can
    /// create these independently with `gh-stack` or the web UI, so this
    /// blocks stacks jjpr never opted into.
    ///
    /// Caught before the merge rather than at the `403` on purpose: jjpr merges
    /// bottom-up in sequence, so discovering it mid-run would leave the stack
    /// half-landed with reconcile already applied to the PRs below.
    NativeStack {
        pr_number: u64,
        stack_number: u64,
        /// 1-based from the bottom. Also the number of PRs a merge here lands.
        position: u32,
        size: u32,
    },
}

impl BlockReason {
    /// Transient reasons that may resolve without user action (worth watching).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::ChecksPending | Self::MergeabilityUnknown | Self::ConcurrentModification
        )
    }
}

/// Merge status for a single segment in the stack.
#[derive(Debug, Clone)]
pub enum PrMergeStatus {
    Mergeable {
        bookmark_name: String,
        pr: PullRequest,
    },
    Blocked {
        bookmark_name: String,
        pr: Option<PullRequest>,
        reasons: Vec<BlockReason>,
    },
    AlreadyMerged {
        bookmark_name: String,
        pr_number: u64,
    },
}

/// Options controlling merge eligibility checks.
#[derive(Debug, Clone)]
pub struct MergeOptions {
    pub merge_method: MergeMethod,
    pub required_approvals: u32,
    pub require_ci_pass: bool,
    pub reconcile_strategy: crate::config::ReconcileStrategy,
    pub ready: bool,
}

/// The full merge plan for a stack.
#[derive(Debug)]
pub struct MergePlan {
    pub actions: Vec<PrMergeStatus>,
    pub repo_info: RepoInfo,
    pub forge_kind: ForgeKind,
    pub default_branch: String,
    pub remote_name: String,
    pub options: MergeOptions,
    /// If the stack is based on a foreign branch, retarget the bottom PR here after merge.
    pub stack_base: Option<String>,
    pub stack_nav: crate::config::StackNavMode,
}

/// Fetch, over the per-PR endpoints, exactly the fields this merge will consult.
///
/// `require_ci_pass` gates the CI lookup the same way the evaluation below does:
/// without it the result is never read, and fetching it anyway would spend
/// requests per PR filling a field that gets dropped.
fn fetch_for_merge(
    github: &dyn Forge,
    repo_info: &RepoInfo,
    pr: &PullRequest,
    options: &MergeOptions,
) -> PrStatusBundle {
    PrStatusBundle {
        mergeability: github
            .get_pr_mergeability(&repo_info.owner, &repo_info.repo, pr.number)
            .ok(),
        checks: options
            .require_ci_pass
            .then(|| {
                github
                    .get_pr_checks_status(&repo_info.owner, &repo_info.repo, pr.checks_ref())
                    .ok()
            })
            .flatten(),
        reviews: github
            .get_pr_reviews(&repo_info.owner, &repo_info.repo, pr.number)
            .ok(),
    }
}

/// Evaluate a single bookmark's merge readiness against current forge state.
///
/// `prefetched` is the forge's batched answer for this PR when it had one. It is
/// only ever an optimization: absent, every field is fetched here instead, and
/// the outcome is the same either way. Errors and prefetch gaps both read as
/// "unknown", which blocks rather than waves the merge through.
pub fn evaluate_segment(
    github: &dyn Forge,
    bookmark_name: &str,
    repo_info: &RepoInfo,
    pr_map: &HashMap<String, PullRequest>,
    options: &MergeOptions,
    prefetched: Option<&PrStatusBundle>,
) -> Result<PrMergeStatus> {
    let Some(pr) = pr_map.get(bookmark_name).cloned() else {
        // No open PR — check if it was already merged
        match github.find_merged_pr(&repo_info.owner, &repo_info.repo, bookmark_name) {
            Ok(Some(merged_pr)) => {
                return Ok(PrMergeStatus::AlreadyMerged {
                    bookmark_name: bookmark_name.to_string(),
                    pr_number: merged_pr.number,
                });
            }
            Ok(None) => {
                return Ok(PrMergeStatus::Blocked {
                    bookmark_name: bookmark_name.to_string(),
                    pr: None,
                    reasons: vec![BlockReason::NoPr],
                });
            }
            Err(e) => {
                return Err(e).context(format!(
                    "failed to check merged status for '{bookmark_name}'"
                ));
            }
        }
    };

    // Before anything else, and before any mutation: a stacked PR cannot be
    // merged through the endpoint jjpr uses, so marking it ready or spending
    // mergeability/CI/review requests on it would all be wasted. Returning here
    // keeps the refusal cheap and keeps jjpr from touching a PR it won't merge.
    if let Some(stack) = &pr.stack {
        return Ok(PrMergeStatus::Blocked {
            bookmark_name: bookmark_name.to_string(),
            reasons: vec![BlockReason::NativeStack {
                pr_number: pr.number,
                stack_number: stack.number,
                position: stack.position,
                size: stack.size,
            }],
            pr: Some(pr),
        });
    }

    let mut reasons = Vec::new();
    let mut prefetched = prefetched;

    if pr.draft {
        if options.ready {
            github.mark_pr_ready(&repo_info.owner, &repo_info.repo, pr.number)?;
            // Anything batched for this PR predates the mutation. Reading it now
            // would describe the draft the PR no longer is, so drop it and let
            // the reads below happen after the change, as they always have.
            prefetched = None;
        } else {
            reasons.push(BlockReason::Draft);
        }
    }

    // Fall back to the per-PR endpoints when the forge could not batch, or when
    // marking the PR ready just invalidated what it did batch.
    let owned;
    let status = match prefetched {
        Some(bundle) => bundle,
        None => {
            owned = fetch_for_merge(github, repo_info, &pr, options);
            &owned
        }
    };

    // A field the forge could not answer blocks the merge rather than silently
    // skipping the check.
    match &status.mergeability {
        Some(mergeability) => match mergeability.mergeable {
            Some(false) => reasons.push(BlockReason::Conflicted),
            None => reasons.push(BlockReason::MergeabilityUnknown),
            Some(true) => {}
        },
        None => reasons.push(BlockReason::MergeabilityUnknown),
    }

    if options.require_ci_pass {
        match &status.checks {
            Some(ChecksStatus::Fail) => reasons.push(BlockReason::ChecksFailing),
            Some(ChecksStatus::Pending) => reasons.push(BlockReason::ChecksPending),
            Some(ChecksStatus::Pass) => {}
            // No checks exist for this commit — CI hasn't started yet.
            Some(ChecksStatus::None) => reasons.push(BlockReason::ChecksPending),
            None => reasons.push(BlockReason::ChecksPending),
        }
    }

    match &status.reviews {
        Some(reviews) => {
            if reviews.changes_requested {
                reasons.push(BlockReason::ChangesRequested);
            }
            if reviews.approved_count < options.required_approvals {
                reasons.push(BlockReason::InsufficientApprovals {
                    have: reviews.approved_count,
                    need: options.required_approvals,
                });
            }
        }
        None => {
            if options.required_approvals > 0 {
                reasons.push(BlockReason::InsufficientApprovals {
                    have: 0,
                    need: options.required_approvals,
                });
            }
        }
    }

    if reasons.is_empty() {
        Ok(PrMergeStatus::Mergeable {
            bookmark_name: bookmark_name.to_string(),
            pr,
        })
    } else {
        Ok(PrMergeStatus::Blocked {
            bookmark_name: bookmark_name.to_string(),
            pr: Some(pr),
            reasons,
        })
    }
}

/// Build a merge plan by checking each segment's PR status bottom-to-top.
/// Stops evaluating after the first blocked segment.
pub fn create_merge_plan(
    github: &dyn Forge,
    segments: &[NarrowedSegment],
    repo_info: &RepoInfo,
    forge_kind: ForgeKind,
    default_branch: &str,
    remote_name: &str,
    options: &MergeOptions,
    stack_base: Option<&str>,
    stack_nav: crate::config::StackNavMode,
) -> Result<MergePlan> {
    let all_open_prs = github.list_open_prs(&repo_info.owner, &repo_info.repo)?;
    let pr_map = crate::forge::build_pr_map(all_open_prs, &repo_info.owner);

    // One batched read for the whole stack, where the forge can do it. Where it
    // cannot this is empty and each segment falls back to its own requests,
    // which keeps the early exit below honest: a stack blocked at its first
    // segment must not pay for the segments the plan never reaches. On a forge
    // that batches, that prefetch is a single request, so reaching past the
    // block costs less than evaluating one segment used to.
    let stack_prs: Vec<&PullRequest> = segments
        .iter()
        .filter_map(|segment| pr_map.get(&segment.bookmark.name))
        .collect();
    let prefetched = crate::forge::status::prefetch(github, repo_info, &stack_prs);

    let mut actions = Vec::new();

    for segment in segments {
        let bundle = pr_map
            .get(&segment.bookmark.name)
            .and_then(|pr| prefetched.get(&pr.number));
        let status = evaluate_segment(
            github, &segment.bookmark.name, repo_info, &pr_map, options, bundle,
        )?;
        let is_blocked = matches!(&status, PrMergeStatus::Blocked { .. });
        actions.push(status);
        if is_blocked {
            break;
        }
    }

    Ok(MergePlan {
        actions,
        repo_info: repo_info.clone(),
        forge_kind,
        default_branch: default_branch.to_string(),
        remote_name: remote_name.to_string(),
        options: options.clone(),
        stack_base: stack_base.map(|s| s.to_string()),
        stack_nav,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::{IssueComment, PrMergeability, PrState, PullRequestRef, ReviewSummary};
    use crate::jj::types::{Bookmark, LogEntry};
    use std::collections::HashMap;

    fn make_segment(name: &str) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.to_string(),
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![LogEntry {
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                author_name: "Test".to_string(),
                author_email: "test@test.com".to_string(),
                description: format!("Add {name}"),
                description_first_line: format!("Add {name}"),
                parents: vec![],
                local_bookmarks: vec![name.to_string()],
                remote_bookmarks: vec![],
                is_working_copy: false,
                conflict: false,
                empty: false,
            }],
            merge_source_names: vec![],
        }
    }

    fn make_pr(name: &str, number: u64) -> PullRequest {
        PullRequest {
            number,
            html_url: format!("https://github.com/o/r/pull/{number}"),
            title: format!("Add {name}"),
            body: None,
            base: PullRequestRef {
                ref_name: "main".to_string(),
                label: String::new(),
                sha: String::new(),
            },
            head: PullRequestRef {
                ref_name: name.to_string(),
                label: String::new(),
                sha: format!("sha_{name}"),
            },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
            author: String::new(),
            stack: None,
        }
    }

    /// A forge that records every per-PR read, so a test can prove a prefetch
    /// actually replaced them rather than merely sitting alongside them.
    struct CountingForge {
        inner: StubGitHub,
        mergeability_calls: std::sync::Mutex<Vec<u64>>,
        checks_calls: std::sync::Mutex<Vec<String>>,
        reviews_calls: std::sync::Mutex<Vec<u64>>,
        batch: Option<HashMap<u64, PrStatusBundle>>,
    }

    impl CountingForge {
        fn new(inner: StubGitHub, batch: Option<HashMap<u64, PrStatusBundle>>) -> Self {
            Self {
                inner,
                mergeability_calls: std::sync::Mutex::new(Vec::new()),
                checks_calls: std::sync::Mutex::new(Vec::new()),
                reviews_calls: std::sync::Mutex::new(Vec::new()),
                batch,
            }
        }
        fn per_pr_reads(&self) -> usize {
            self.mergeability_calls.lock().unwrap().len()
                + self.checks_calls.lock().unwrap().len()
                + self.reviews_calls.lock().unwrap().len()
        }
    }

    impl Forge for CountingForge {
        fn batch_pr_status(
            &self,
            _o: &str,
            _r: &str,
            prs: &[(u64, String)],
        ) -> Option<HashMap<u64, PrStatusBundle>> {
            let batch = self.batch.as_ref()?;
            Some(
                prs.iter()
                    .filter_map(|(n, _)| batch.get(n).map(|b| (*n, b.clone())))
                    .collect(),
            )
        }
        fn get_pr_mergeability(&self, o: &str, r: &str, n: u64) -> Result<PrMergeability> {
            self.mergeability_calls.lock().unwrap().push(n);
            self.inner.get_pr_mergeability(o, r, n)
        }
        fn get_pr_checks_status(&self, o: &str, r: &str, h: &str) -> Result<ChecksStatus> {
            self.checks_calls.lock().unwrap().push(h.to_string());
            self.inner.get_pr_checks_status(o, r, h)
        }
        fn get_pr_reviews(&self, o: &str, r: &str, n: u64) -> Result<ReviewSummary> {
            self.reviews_calls.lock().unwrap().push(n);
            self.inner.get_pr_reviews(o, r, n)
        }
        fn list_open_prs(&self, o: &str, r: &str) -> Result<Vec<PullRequest>> {
            self.inner.list_open_prs(o, r)
        }
        fn find_merged_pr(&self, o: &str, r: &str, h: &str) -> Result<Option<PullRequest>> {
            self.inner.find_merged_pr(o, r, h)
        }
        fn mark_pr_ready(&self, o: &str, r: &str, n: u64) -> Result<()> {
            self.inner.mark_pr_ready(o, r, n)
        }
        fn create_pr(&self, o: &str, r: &str, t: &str, b: &str, h: &str, ba: &str, d: bool) -> Result<PullRequest> {
            self.inner.create_pr(o, r, t, b, h, ba, d)
        }
        fn update_pr_base(&self, o: &str, r: &str, n: u64, b: &str) -> Result<()> {
            self.inner.update_pr_base(o, r, n, b)
        }
        fn request_reviewers(&self, o: &str, r: &str, n: u64, v: &[String]) -> Result<()> {
            self.inner.request_reviewers(o, r, n, v)
        }
        fn list_comments(&self, o: &str, r: &str, n: u64) -> Result<Vec<IssueComment>> {
            self.inner.list_comments(o, r, n)
        }
        fn create_comment(&self, o: &str, r: &str, n: u64, b: &str) -> Result<IssueComment> {
            self.inner.create_comment(o, r, n, b)
        }
        fn update_comment(&self, o: &str, r: &str, c: u64, b: &str) -> Result<()> {
            self.inner.update_comment(o, r, c, b)
        }
        fn update_pr_body(&self, o: &str, r: &str, n: u64, b: &str) -> Result<()> {
            self.inner.update_pr_body(o, r, n, b)
        }
        fn get_authenticated_user(&self) -> Result<String> {
            self.inner.get_authenticated_user()
        }
        fn merge_pr(&self, o: &str, r: &str, n: u64, m: MergeMethod) -> Result<()> {
            self.inner.merge_pr(o, r, n, m)
        }
        fn get_pr_state(&self, o: &str, r: &str, n: u64) -> Result<PrState> {
            self.inner.get_pr_state(o, r, n)
        }
    }

    fn green_bundle() -> PrStatusBundle {
        PrStatusBundle {
            mergeability: Some(PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".to_string(),
            }),
            checks: Some(ChecksStatus::Pass),
            reviews: Some(ReviewSummary {
                approved_count: 1,
                changes_requested: false,
            }),
        }
    }

    // The prefetch must be a pure optimization: same decision, fewer requests.
    #[test]
    fn a_prefetched_bundle_replaces_the_per_pr_reads() {
        let batch = HashMap::from([(1, green_bundle())]);
        let forge = CountingForge::new(StubGitHub::new().with_mergeable_pr("auth", 1), Some(batch));
        let plan = create_merge_plan(
            &forge, &[make_segment("auth")], &repo_info(), ForgeKind::GitHub,
            "main", "origin", &default_options(), None,
            crate::config::StackNavMode::Comment,
        )
        .unwrap();
        assert!(matches!(plan.actions[0], PrMergeStatus::Mergeable { .. }));
        assert_eq!(forge.per_pr_reads(), 0, "the batch should have answered everything");
    }

    #[test]
    fn without_a_batch_path_the_per_pr_reads_still_happen() {
        let forge = CountingForge::new(StubGitHub::new().with_mergeable_pr("auth", 1), None);
        let plan = create_merge_plan(
            &forge, &[make_segment("auth")], &repo_info(), ForgeKind::GitHub,
            "main", "origin", &default_options(), None,
            crate::config::StackNavMode::Comment,
        )
        .unwrap();
        assert!(matches!(plan.actions[0], PrMergeStatus::Mergeable { .. }));
        assert_eq!(forge.per_pr_reads(), 3, "mergeability + checks + reviews");
    }

    // Users can register jjpr's PRs as a native stack with `gh-stack` or the web
    // UI at any time, so merge has to notice before it starts rather than at the
    // 403 partway up the stack.
    #[test]
    fn a_stacked_pr_is_blocked_before_any_per_pr_read() {
        let forge = CountingForge::new(
            StubGitHub::new().with_stacked_pr("auth", 1, 223, 2, 4),
            None,
        );
        let plan = create_merge_plan(
            &forge, &[make_segment("auth")], &repo_info(), ForgeKind::GitHub,
            "main", "origin", &default_options(), None,
            crate::config::StackNavMode::Comment,
        )
        .unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => assert_eq!(
                reasons,
                &vec![BlockReason::NativeStack {
                    pr_number: 1,
                    stack_number: 223,
                    position: 2,
                    size: 4,
                }],
                "stack membership should be the sole, dispositive reason"
            ),
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(
            forge.per_pr_reads(),
            0,
            "a PR we cannot merge should cost no mergeability/CI/review requests"
        );
    }

    // The check must precede the draft branch: marking a stacked PR ready would
    // mutate a PR jjpr is about to refuse. StubGitHub::mark_pr_ready is
    // unimplemented!(), so a wrong ordering panics here rather than passing.
    #[test]
    fn a_stacked_draft_pr_is_never_marked_ready() {
        let forge = StubGitHub::new().with_stacked_pr("auth", 1, 223, 1, 2);
        let options = MergeOptions { ready: true, ..default_options() };
        let plan = create_merge_plan(
            &forge, &[make_segment("auth")], &repo_info(), ForgeKind::GitHub,
            "main", "origin", &options, None,
            crate::config::StackNavMode::Comment,
        )
        .unwrap();
        assert!(matches!(
            &plan.actions[0],
            PrMergeStatus::Blocked { reasons, .. }
                if matches!(reasons[..], [BlockReason::NativeStack { .. }])
        ));
    }

    // Guard the common path: an ordinary PR must be unaffected by the new field.
    #[test]
    fn an_unstacked_pr_is_still_mergeable() {
        let forge = StubGitHub::new().with_mergeable_pr("auth", 1);
        let plan = create_merge_plan(
            &forge, &[make_segment("auth")], &repo_info(), ForgeKind::GitHub,
            "main", "origin", &default_options(), None,
            crate::config::StackNavMode::Comment,
        )
        .unwrap();
        assert!(matches!(plan.actions[0], PrMergeStatus::Mergeable { .. }));
    }

    #[test]
    fn batched_and_unbatched_reach_the_same_verdict() {
        let blocked = PrStatusBundle {
            mergeability: Some(PrMergeability {
                mergeable: Some(false),
                mergeable_state: "dirty".to_string(),
            }),
            checks: Some(ChecksStatus::Fail),
            reviews: Some(ReviewSummary {
                approved_count: 0,
                changes_requested: true,
            }),
        };
        let stub = || {
            let mut s = StubGitHub::new().with_mergeable_pr("auth", 1);
            s.mergeability.insert(1, PrMergeability {
                mergeable: Some(false),
                mergeable_state: "dirty".to_string(),
            });
            s.checks.insert("sha_auth".to_string(), ChecksStatus::Fail);
            s.reviews.insert(1, ReviewSummary { approved_count: 0, changes_requested: true });
            s
        };
        let opts = default_options();
        let plan_batched = create_merge_plan(
            &CountingForge::new(stub(), Some(HashMap::from([(1, blocked)]))),
            &[make_segment("auth")], &repo_info(), ForgeKind::GitHub, "main", "origin",
            &opts, None, crate::config::StackNavMode::Comment,
        ).unwrap();
        let plan_direct = create_merge_plan(
            &CountingForge::new(stub(), None),
            &[make_segment("auth")], &repo_info(), ForgeKind::GitHub, "main", "origin",
            &opts, None, crate::config::StackNavMode::Comment,
        ).unwrap();
        assert_eq!(
            format!("{:?}", plan_batched.actions),
            format!("{:?}", plan_direct.actions),
            "the batch must not change the verdict, only how it was reached",
        );
    }

    #[test]
    fn a_gap_in_the_batch_blocks_rather_than_passes() {
        // An empty bundle is what a forge that could not answer looks like. It
        // must fail closed, never wave the merge through.
        let batch = HashMap::from([(1, PrStatusBundle::default())]);
        let forge = CountingForge::new(StubGitHub::new().with_mergeable_pr("auth", 1), Some(batch));
        let plan = create_merge_plan(
            &forge, &[make_segment("auth")], &repo_info(), ForgeKind::GitHub,
            "main", "origin", &default_options(), None,
            crate::config::StackNavMode::Comment,
        )
        .unwrap();
        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::MergeabilityUnknown));
                assert!(reasons.contains(&BlockReason::ChecksPending));
                assert!(reasons.iter().any(|r| matches!(r, BlockReason::InsufficientApprovals { .. })));
            }
            other => panic!("an unanswered PR must block, got {other:?}"),
        }
    }

    #[test]
    fn ci_is_not_fetched_when_it_is_not_required() {
        // Without require_ci_pass the result is never read, so paying for it
        // would be pure waste on the per-PR path.
        let mut opts = default_options();
        opts.require_ci_pass = false;
        let forge = CountingForge::new(StubGitHub::new().with_mergeable_pr("auth", 1), None);
        create_merge_plan(
            &forge, &[make_segment("auth")], &repo_info(), ForgeKind::GitHub,
            "main", "origin", &opts, None, crate::config::StackNavMode::Comment,
        )
        .unwrap();
        assert!(
            forge.checks_calls.lock().unwrap().is_empty(),
            "CI must not be fetched when it is not required",
        );
    }

    fn default_options() -> MergeOptions {
        MergeOptions {
            merge_method: MergeMethod::Squash,
            required_approvals: 1,
            require_ci_pass: true,
            reconcile_strategy: crate::config::ReconcileStrategy::Rebase,
            ready: false,
        }
    }

    fn repo_info() -> RepoInfo {
        RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        }
    }

    struct StubGitHub {
        open_prs: Vec<PullRequest>,
        merged_prs: HashMap<String, PullRequest>,
        mergeability: HashMap<u64, PrMergeability>,
        checks: HashMap<String, ChecksStatus>,
        reviews: HashMap<u64, ReviewSummary>,
    }

    impl StubGitHub {
        fn new() -> Self {
            Self {
                open_prs: vec![],
                merged_prs: HashMap::new(),
                mergeability: HashMap::new(),
                checks: HashMap::new(),
                reviews: HashMap::new(),
            }
        }

        fn with_mergeable_pr(mut self, name: &str, number: u64) -> Self {
            self.open_prs.push(make_pr(name, number));
            self.mergeability.insert(number, PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".to_string(),
            });
            self.checks.insert(format!("sha_{name}"), ChecksStatus::Pass);
            self.reviews.insert(number, ReviewSummary {
                approved_count: 1,
                changes_requested: false,
            });
            self
        }

        /// A PR that is green on every axis but belongs to a native stack —
        /// isolating stack membership as the only thing that can block it.
        fn with_stacked_pr(mut self, name: &str, number: u64, stack: u64, pos: u32, size: u32) -> Self {
            self = self.with_mergeable_pr(name, number);
            let pr = self.open_prs.last_mut().expect("just pushed");
            pr.draft = true; // also proves the stack check runs before mark_pr_ready
            pr.stack = Some(crate::forge::types::PrStackRef {
                number: stack,
                id: 1,
                position: pos,
                size,
                base: Some(PullRequestRef {
                    ref_name: "main".to_string(),
                    label: String::new(),
                    sha: String::new(),
                }),
            });
            self
        }
    }

    impl Forge for StubGitHub {
        fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
            Ok(self.open_prs.clone())
        }
        fn find_merged_pr(&self, _o: &str, _r: &str, head: &str) -> Result<Option<PullRequest>> {
            Ok(self.merged_prs.get(head).cloned())
        }
        fn get_pr_mergeability(&self, _o: &str, _r: &str, n: u64) -> Result<PrMergeability> {
            self.mergeability
                .get(&n)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no mergeability stub for PR #{n}"))
        }
        fn get_pr_checks_status(&self, _o: &str, _r: &str, head: &str) -> Result<ChecksStatus> {
            self.checks
                .get(head)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no checks stub for {head}"))
        }
        fn get_pr_reviews(&self, _o: &str, _r: &str, n: u64) -> Result<ReviewSummary> {
            self.reviews
                .get(&n)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no reviews stub for PR #{n}"))
        }
        fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
        fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
        fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
        fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
        fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
        fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
        fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
        fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
        fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
            Ok(PrState { merged: false, state: "open".to_string() })
        }
    }

    #[test]
    fn test_all_mergeable() {
        let gh = StubGitHub::new()
            .with_mergeable_pr("auth", 1)
            .with_mergeable_pr("profile", 2);

        let segments = vec![make_segment("auth"), make_segment("profile")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "auth"));
        assert!(matches!(&plan.actions[1], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "profile"));
    }

    #[test]
    fn test_blocked_by_draft() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.open_prs[0].draft = true;

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        assert_eq!(plan.actions.len(), 1);
        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::Draft));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_blocked_by_failing_ci() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::Fail);

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::ChecksFailing));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_blocked_by_pending_ci() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::Pending);

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::ChecksPending));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_blocked_by_insufficient_approvals() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.reviews.insert(1, ReviewSummary {
            approved_count: 0,
            changes_requested: false,
        });

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(matches!(
                    reasons.as_slice(),
                    [BlockReason::InsufficientApprovals { have: 0, need: 1 }]
                ));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_blocked_by_changes_requested() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.reviews.insert(1, ReviewSummary {
            approved_count: 1,
            changes_requested: true,
        });

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::ChangesRequested));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_blocked_by_conflict() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.mergeability.insert(1, PrMergeability {
            mergeable: Some(false),
            mergeable_state: "dirty".to_string(),
        });

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::Conflicted));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_blocked_by_unknown_mergeability() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.mergeability.insert(1, PrMergeability {
            mergeable: None,
            mergeable_state: "unknown".to_string(),
        });

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::MergeabilityUnknown));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_no_pr_blocks() {
        let gh = StubGitHub::new();

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        assert_eq!(plan.actions.len(), 1);
        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::NoPr));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_already_merged_then_mergeable() {
        let mut gh = StubGitHub::new().with_mergeable_pr("profile", 2);
        gh.merged_prs.insert("auth".to_string(), PullRequest {
            number: 1,
            merged_at: Some("2024-01-01T00:00:00Z".to_string()),
            ..make_pr("auth", 1)
        });

        let segments = vec![make_segment("auth"), make_segment("profile")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(&plan.actions[0], PrMergeStatus::AlreadyMerged { pr_number: 1, .. }));
        assert!(matches!(&plan.actions[1], PrMergeStatus::Mergeable { .. }));
    }

    #[test]
    fn test_blocked_stops_evaluation() {
        let mut gh = StubGitHub::new()
            .with_mergeable_pr("auth", 1)
            .with_mergeable_pr("settings", 3);
        // auth is draft → blocked. profile and settings should not be evaluated.
        gh.open_prs[0].draft = true;

        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        // Only auth should appear — the rest are not evaluated
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PrMergeStatus::Blocked { bookmark_name, .. } if bookmark_name == "auth"));
    }

    #[test]
    fn test_ci_not_checked_when_disabled() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::Fail);

        let mut options = default_options();
        options.require_ci_pass = false;

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &options, None, crate::config::StackNavMode::Comment).unwrap();

        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { .. }));
    }

    #[test]
    fn test_no_checks_blocks_when_ci_required() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::None);

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        assert!(matches!(&plan.actions[0], PrMergeStatus::Blocked { .. }));
    }

    #[test]
    fn test_no_checks_allowed_when_ci_not_required() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::None);

        let mut options = default_options();
        options.require_ci_pass = false;
        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &options, None, crate::config::StackNavMode::Comment).unwrap();

        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { .. }));
    }

    #[test]
    fn test_multiple_block_reasons_collected() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.open_prs[0].draft = true;
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::Fail);
        gh.reviews.insert(1, ReviewSummary {
            approved_count: 0,
            changes_requested: true,
        });

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::Draft));
                assert!(reasons.contains(&BlockReason::ChecksFailing));
                assert!(reasons.contains(&BlockReason::ChangesRequested));
                assert!(reasons.iter().any(|r| matches!(r, BlockReason::InsufficientApprovals { .. })));
                assert_eq!(reasons.len(), 4);
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn test_api_error_blocks_mergeability() {
        // If mergeability API fails, PR should be blocked (not silently marked mergeable)
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.mergeability.remove(&1); // remove stub so it returns Err

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::MergeabilityUnknown));
            }
            other => panic!("expected Blocked due to API error, got {other:?}"),
        }
    }

    #[test]
    fn test_api_error_blocks_ci_check() {
        // If CI checks API fails, PR should be blocked with pending (not silently skipped)
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.remove("sha_auth"); // remove stub so it returns Err

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.contains(&BlockReason::ChecksPending));
            }
            other => panic!("expected Blocked due to CI API error, got {other:?}"),
        }
    }

    #[test]
    fn test_api_error_blocks_reviews() {
        // If reviews API fails, PR should be blocked (not silently skipped)
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.reviews.remove(&1); // remove stub so it returns Err

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        match &plan.actions[0] {
            PrMergeStatus::Blocked { reasons, .. } => {
                assert!(reasons.iter().any(|r| matches!(r, BlockReason::InsufficientApprovals { .. })));
            }
            other => panic!("expected Blocked due to reviews API error, got {other:?}"),
        }
    }

    #[test]
    fn test_api_error_with_zero_approvals_does_not_block() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.reviews.remove(&1); // API error

        let mut options = default_options();
        options.required_approvals = 0;

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &options, None, crate::config::StackNavMode::Comment).unwrap();

        assert!(
            matches!(&plan.actions[0], PrMergeStatus::Mergeable { .. }),
            "zero required_approvals + API error should not block: {:?}",
            plan.actions[0]
        );
    }

    #[test]
    fn test_find_merged_pr_error_propagates() {
        struct ErrorGitHub;
        impl Forge for ErrorGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> { Ok(vec![]) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> {
                anyhow::bail!("network timeout")
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("auth")];
        let err = create_merge_plan(&ErrorGitHub, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("network timeout"), "should propagate the underlying error: {msg}");
        assert!(msg.contains("auth"), "should mention the bookmark name: {msg}");
    }

    #[test]
    fn test_three_segment_all_mergeable() {
        let gh = StubGitHub::new()
            .with_mergeable_pr("auth", 1)
            .with_mergeable_pr("profile", 2)
            .with_mergeable_pr("settings", 3);

        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), ForgeKind::GitHub, "main", "origin", &default_options(), None, crate::config::StackNavMode::Comment).unwrap();

        assert_eq!(plan.actions.len(), 3);
        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "auth"));
        assert!(matches!(&plan.actions[1], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "profile"));
        assert!(matches!(&plan.actions[2], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "settings"));
    }
}
