use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::forge::types::{ChecksStatus, MergeMethod, PullRequest, RepoInfo};
use crate::forge::Forge;
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
}

/// The full merge plan for a stack.
#[derive(Debug)]
pub struct MergePlan {
    pub actions: Vec<PrMergeStatus>,
    pub repo_info: RepoInfo,
    pub default_branch: String,
    pub remote_name: String,
    pub options: MergeOptions,
    /// If the stack is based on a foreign branch, retarget the bottom PR here after merge.
    pub stack_base: Option<String>,
}

/// Evaluate a single bookmark's merge readiness against current GitHub state.
pub fn evaluate_segment(
    github: &dyn Forge,
    bookmark_name: &str,
    repo_info: &RepoInfo,
    pr_map: &HashMap<String, PullRequest>,
    options: &MergeOptions,
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

    let mut reasons = Vec::new();

    if pr.draft {
        reasons.push(BlockReason::Draft);
    }

    // API errors block the merge rather than silently skipping the check
    match github.get_pr_mergeability(&repo_info.owner, &repo_info.repo, pr.number) {
        Ok(mergeability) => match mergeability.mergeable {
            Some(false) => reasons.push(BlockReason::Conflicted),
            None => reasons.push(BlockReason::MergeabilityUnknown),
            Some(true) => {}
        },
        Err(_) => reasons.push(BlockReason::MergeabilityUnknown),
    }

    if options.require_ci_pass {
        match github.get_pr_checks_status(
            &repo_info.owner,
            &repo_info.repo,
            &pr.head.ref_name,
        ) {
            Ok(ChecksStatus::Fail) => reasons.push(BlockReason::ChecksFailing),
            Ok(ChecksStatus::Pending) => reasons.push(BlockReason::ChecksPending),
            Ok(ChecksStatus::Pass | ChecksStatus::None) => {}
            Err(_) => reasons.push(BlockReason::ChecksPending),
        }
    }

    match github.get_pr_reviews(&repo_info.owner, &repo_info.repo, pr.number) {
        Ok(reviews) => {
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
        Err(_) => {
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
    default_branch: &str,
    remote_name: &str,
    options: &MergeOptions,
    stack_base: Option<&str>,
) -> Result<MergePlan> {
    let all_open_prs = github.list_open_prs(&repo_info.owner, &repo_info.repo)?;
    let pr_map = crate::forge::build_pr_map(all_open_prs, &repo_info.owner);

    let mut actions = Vec::new();

    for segment in segments {
        let status = evaluate_segment(
            github, &segment.bookmark.name, repo_info, &pr_map, options,
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
        default_branch: default_branch.to_string(),
        remote_name: remote_name.to_string(),
        options: options.clone(),
        stack_base: stack_base.map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::{IssueComment, PrMergeability, PullRequestRef, ReviewSummary};
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
            }],
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
            },
            head: PullRequestRef {
                ref_name: name.to_string(),
                label: String::new(),
            },
            draft: false,
            node_id: String::new(),
            merged_at: None,
        }
    }

    fn default_options() -> MergeOptions {
        MergeOptions {
            merge_method: MergeMethod::Squash,
            required_approvals: 1,
            require_ci_pass: true,
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
            self.checks.insert(name.to_string(), ChecksStatus::Pass);
            self.reviews.insert(number, ReviewSummary {
                approved_count: 1,
                changes_requested: false,
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
    }

    #[test]
    fn test_all_mergeable() {
        let gh = StubGitHub::new()
            .with_mergeable_pr("auth", 1)
            .with_mergeable_pr("profile", 2);

        let segments = vec![make_segment("auth"), make_segment("profile")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "auth"));
        assert!(matches!(&plan.actions[1], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "profile"));
    }

    #[test]
    fn test_blocked_by_draft() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.open_prs[0].draft = true;

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        gh.checks.insert("auth".to_string(), ChecksStatus::Fail);

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        gh.checks.insert("auth".to_string(), ChecksStatus::Pending);

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

        // Only auth should appear — the rest are not evaluated
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(&plan.actions[0], PrMergeStatus::Blocked { bookmark_name, .. } if bookmark_name == "auth"));
    }

    #[test]
    fn test_ci_not_checked_when_disabled() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.insert("auth".to_string(), ChecksStatus::Fail);

        let mut options = default_options();
        options.require_ci_pass = false;

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &options, None).unwrap();

        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { .. }));
    }

    #[test]
    fn test_no_checks_configured_allows_merge() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.checks.insert("auth".to_string(), ChecksStatus::None);

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { .. }));
    }

    #[test]
    fn test_multiple_block_reasons_collected() {
        let mut gh = StubGitHub::new().with_mergeable_pr("auth", 1);
        gh.open_prs[0].draft = true;
        gh.checks.insert("auth".to_string(), ChecksStatus::Fail);
        gh.reviews.insert(1, ReviewSummary {
            approved_count: 0,
            changes_requested: true,
        });

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        gh.checks.remove("auth"); // remove stub so it returns Err

        let segments = vec![make_segment("auth")];
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &options, None).unwrap();

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
        }

        let segments = vec![make_segment("auth")];
        let err = create_merge_plan(&ErrorGitHub, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap_err();
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
        let plan = create_merge_plan(&gh, &segments, &repo_info(), "main", "origin", &default_options(), None).unwrap();

        assert_eq!(plan.actions.len(), 3);
        assert!(matches!(&plan.actions[0], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "auth"));
        assert!(matches!(&plan.actions[1], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "profile"));
        assert!(matches!(&plan.actions[2], PrMergeStatus::Mergeable { bookmark_name, .. } if bookmark_name == "settings"));
    }
}
