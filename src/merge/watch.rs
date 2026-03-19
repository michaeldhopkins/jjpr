use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::forge::types::PullRequest;
use crate::forge::{Forge, ForgeKind};
use crate::jj::types::NarrowedSegment;
use crate::jj::Jj;

use super::execute::{
    format_block_reason, merge_with_retry, reconcile_after_merge, BlockedPr, LocalDivergenceWarning,
    MergeResult, MergedPr, SkippedMergedPr,
};
use super::plan::{evaluate_segment, BlockReason, MergePlan, PrMergeStatus};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

/// Sleep in small increments so Ctrl+C is responsive.
/// Returns `true` if interrupted by the shutdown flag.
fn interruptible_sleep(duration: Duration, shutdown: &AtomicBool) -> bool {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        if shutdown.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    false
}

fn format_resolved_reason(reason: &BlockReason) -> &'static str {
    match reason {
        BlockReason::ChecksPending | BlockReason::ChecksFailing => "CI now passing",
        BlockReason::Draft => "No longer a draft",
        BlockReason::InsufficientApprovals { .. } => "Approval received",
        BlockReason::ChangesRequested => "Changes-requested resolved",
        BlockReason::Conflicted => "Conflicts resolved",
        BlockReason::MergeabilityUnknown => "Mergeability computed",
        BlockReason::NoPr => "PR now exists",
    }
}

/// Report status changes between previous and current block reasons.
/// Returns true if any output was printed (i.e., something changed).
fn report_status_changes(
    bookmark: &str,
    prev: Option<&[BlockReason]>,
    current: &[BlockReason],
    fk: ForgeKind,
) -> bool {
    let Some(prev) = prev else {
        // First time blocked — print all reasons
        println!("\n  Waiting for '{bookmark}':");
        for reason in current {
            println!("    - {}", format_block_reason(reason, fk));
        }
        return true;
    };

    if prev == current {
        return false;
    }

    let mut printed = false;

    // Report resolved reasons
    for old in prev {
        if !current.iter().any(|c| std::mem::discriminant(c) == std::mem::discriminant(old)) {
            println!("  {bookmark}: {}", format_resolved_reason(old));
            printed = true;
        }
    }

    // Report new reasons
    for new in current {
        if !prev.iter().any(|p| std::mem::discriminant(p) == std::mem::discriminant(new)) {
            println!("  {bookmark}: {}", format_block_reason(new, fk));
            printed = true;
        }
    }

    // Report approval count changes within InsufficientApprovals
    for new in current {
        if let BlockReason::InsufficientApprovals { have: new_have, need } = new {
            for old in prev {
                if let BlockReason::InsufficientApprovals { have: old_have, .. } = old
                    && new_have != old_have
                {
                    println!("  {bookmark}: Approvals now {new_have}/{need}");
                    printed = true;
                }
            }
        }
    }

    printed
}

fn refresh_pr_map(
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
) -> Result<HashMap<String, PullRequest>> {
    let fresh_prs = forge.list_open_prs(owner, repo)?;
    Ok(crate::forge::build_pr_map(fresh_prs, owner))
}

pub struct WatchOptions {
    pub shutdown: Arc<AtomicBool>,
    pub timeout: Option<Duration>,
    pub poll_interval: Duration,
}

/// Persistent watch loop: evaluates segments, merges when ready, waits when blocked.
pub fn execute_merge_plan_watch(
    jj: &dyn Jj,
    forge: &dyn Forge,
    plan: &MergePlan,
    segments: &[NarrowedSegment],
    opts: WatchOptions,
) -> Result<MergeResult> {
    let shutdown = opts.shutdown;
    let timeout = opts.timeout;
    let poll_interval = opts.poll_interval;
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;
    let fk = plan.forge_kind;

    let mut merged = Vec::new();
    let mut blocked_at = None;
    let mut skipped_merged = Vec::new();
    let mut local_warnings: Vec<LocalDivergenceWarning> = Vec::new();
    let mut local_degraded = false;

    let mut pr_map = refresh_pr_map(forge, owner, repo)?;
    let mut seg_idx = 0;
    let mut prev_reasons: Option<Vec<BlockReason>> = None;
    let mut consecutive_errors: u32 = 0;
    let mut last_heartbeat = Instant::now();
    let deadline = timeout.map(|d| Instant::now() + d);

    while seg_idx < segments.len() {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            println!("\nWatch timed out.");
            break;
        }

        let segment = &segments[seg_idx];
        let status = match evaluate_segment(
            forge,
            &segment.bookmark.name,
            &plan.repo_info,
            &pr_map,
            &plan.options,
        ) {
            Ok(s) => {
                consecutive_errors = 0;
                s
            }
            Err(e) => {
                consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!("  [{now}] Poll error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("  Too many consecutive errors \u{2014} giving up.");
                    break;
                }
                if interruptible_sleep(poll_interval, &shutdown) {
                    println!("\nInterrupted.");
                    break;
                }
                // Refresh PR map and retry
                if let Ok(fresh) = refresh_pr_map(forge, owner, repo) {
                    pr_map = fresh;
                }
                continue;
            }
        };

        let prev_seg_idx = seg_idx;

        match status {
            PrMergeStatus::AlreadyMerged {
                bookmark_name,
                pr_number,
            } => {
                if prev_reasons.is_some() {
                    println!("  {bookmark_name}: Merged externally ({}) \u{2014} moving on",
                        fk.format_ref(pr_number));
                } else {
                    println!("  '{bookmark_name}' ({}) already merged",
                        fk.format_ref(pr_number));
                }
                skipped_merged.push(SkippedMergedPr {
                    bookmark_name,
                    pr_number,
                });
                prev_reasons = None;
                seg_idx += 1;
            }

            PrMergeStatus::Mergeable { bookmark_name, pr } => {
                if prev_reasons.is_some() {
                    println!("  {bookmark_name}: Ready to merge");
                }

                println!(
                    "\n  Merging '{bookmark_name}' ({}, {})...",
                    fk.format_ref(pr.number),
                    plan.options.merge_method
                );
                println!("    {}", pr.html_url);

                merge_with_retry(
                    forge, owner, repo, pr.number, plan.options.merge_method, fk,
                )
                .with_context(|| {
                    format!(
                        "failed to merge {} for '{bookmark_name}'",
                        fk.format_ref(pr.number)
                    )
                })?;

                merged.push(MergedPr {
                    bookmark_name,
                    pr_number: pr.number,
                    html_url: pr.html_url.clone(),
                });

                prev_reasons = None;
                seg_idx += 1;
            }

            PrMergeStatus::Blocked {
                bookmark_name,
                pr: _,
                reasons,
            } => {
                if reasons.iter().any(|r| matches!(r, BlockReason::NoPr)) {
                    println!("\n  Blocked at '{bookmark_name}':");
                    println!("    - No PR exists for this bookmark");
                    blocked_at = Some(BlockedPr {
                        bookmark_name,
                        pr_number: None,
                        reasons,
                    });
                    break;
                }

                let changed = report_status_changes(
                    &bookmark_name,
                    prev_reasons.as_deref(),
                    &reasons,
                    fk,
                );

                if !changed && last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                    let now = local_time_hhmm();
                    let first_reason = reasons
                        .first()
                        .map(|r| format_block_reason(r, fk))
                        .unwrap_or_default();
                    println!("  [{now}] Still waiting for {bookmark_name}: {first_reason}");
                    last_heartbeat = Instant::now();
                }

                if changed {
                    last_heartbeat = Instant::now();
                }

                prev_reasons = Some(reasons);

                if interruptible_sleep(poll_interval, &shutdown) {
                    break;
                }

                if let Ok(fresh) = refresh_pr_map(forge, owner, repo) {
                    pr_map = fresh;
                }
            }
        }

        // Reconcile after any segment advance (merged or already-merged).
        if seg_idx > prev_seg_idx && seg_idx < segments.len() {
            let fresh = reconcile_after_merge(
                jj, forge, segments, prev_seg_idx, plan, fk,
                &mut local_degraded, &mut local_warnings,
            );
            if let Some(fresh_map) = fresh {
                pr_map = fresh_map;
            }
        }
    }

    Ok(MergeResult {
        merged,
        blocked_at,
        skipped_merged,
        local_warnings,
    })
}

fn local_time_hhmm() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    unsafe { libc::localtime_r(&secs, &mut tm) };
    #[cfg(windows)]
    unsafe { libc::localtime_s(&mut tm, &secs) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::forge::types::{
        ChecksStatus, MergeMethod, PrMergeability, PullRequest, PullRequestRef, RepoInfo,
        ReviewSummary,
    };
    use crate::forge::{Forge, ForgeKind};
    use crate::jj::types::{Bookmark, LogEntry, NarrowedSegment};
    use crate::jj::Jj;
    use crate::merge::plan::MergeOptions;

    use anyhow::Result;

    // --- Stubs ---

    struct StubJj;
    impl Jj for StubJj {
        fn git_fetch(&self) -> Result<()> { Ok(()) }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
        fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
        fn get_git_remotes(&self) -> Result<Vec<crate::jj::types::GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
        fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> { Ok(()) }
        fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
        fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { Ok(()) }
        fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { Ok(()) }
        fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
            Ok(vec!["dummy".to_string()])
        }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
    }

    struct ScriptedForge {
        eval_sequence: Mutex<Vec<EvalResult>>,
        open_prs: Mutex<Vec<PullRequest>>,
        merge_calls: Mutex<Vec<u64>>,
        merged_prs: Mutex<Vec<(String, PullRequest)>>,
    }

    enum EvalResult {
        Mergeable,
        Blocked(Vec<BlockReason>),
    }

    impl ScriptedForge {
        fn new(sequence: Vec<EvalResult>) -> Self {
            Self {
                eval_sequence: Mutex::new(sequence),
                open_prs: Mutex::new(Vec::new()),
                merge_calls: Mutex::new(Vec::new()),
                merged_prs: Mutex::new(Vec::new()),
            }
        }

        fn with_prs(self, prs: Vec<PullRequest>) -> Self {
            *self.open_prs.lock().expect("poisoned") = prs;
            self
        }

        fn merge_calls(&self) -> Vec<u64> {
            self.merge_calls.lock().expect("poisoned").clone()
        }
    }

    impl Forge for ScriptedForge {
        fn list_open_prs(&self, _owner: &str, _repo: &str) -> Result<Vec<PullRequest>> {
            Ok(self.open_prs.lock().expect("poisoned").clone())
        }

        fn get_pr_mergeability(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
        ) -> Result<PrMergeability> {
            // Return based on scripted sequence
            let mut seq = self.eval_sequence.lock().expect("poisoned");
            let result = if seq.is_empty() {
                return Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".to_string() });
            } else {
                seq.remove(0)
            };
            match result {
                EvalResult::Mergeable => Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".to_string() }),
                EvalResult::Blocked(reasons) => {
                    // Map first reason to appropriate mergeability
                    if reasons.iter().any(|r| matches!(r, BlockReason::Conflicted)) {
                        Ok(PrMergeability { mergeable: Some(false), mergeable_state: "dirty".to_string() })
                    } else if reasons
                        .iter()
                        .any(|r| matches!(r, BlockReason::MergeabilityUnknown))
                    {
                        Ok(PrMergeability { mergeable: None, mergeable_state: "unknown".to_string() })
                    } else {
                        Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".to_string() })
                    }
                }
            }
        }

        fn get_pr_checks_status(
            &self,
            _owner: &str,
            _repo: &str,
            _ref_name: &str,
        ) -> Result<ChecksStatus> {
            // Always pass — the scripted sequence controls blocking via mergeability
            Ok(ChecksStatus::Pass)
        }

        fn get_pr_reviews(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
        ) -> Result<ReviewSummary> {
            Ok(ReviewSummary {
                approved_count: 1,
                changes_requested: false,
            })
        }

        fn merge_pr(
            &self,
            _owner: &str,
            _repo: &str,
            number: u64,
            _method: MergeMethod,
        ) -> Result<()> {
            self.merge_calls.lock().expect("poisoned").push(number);
            Ok(())
        }

        fn create_pr(
            &self, _o: &str, _r: &str, _t: &str, _body: &str, _h: &str, _b: &str, _d: bool,
        ) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
        fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { Ok(()) }
        fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { Ok(()) }
        fn list_comments(&self, _o: &str, _r: &str, _n: u64) -> Result<Vec<crate::forge::IssueComment>> { Ok(vec![]) }
        fn create_comment(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<crate::forge::IssueComment> { unimplemented!() }
        fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { Ok(()) }
        fn get_authenticated_user(&self) -> Result<String> { Ok("user".to_string()) }
        fn find_merged_pr(&self, _o: &str, _r: &str, ref_name: &str) -> Result<Option<PullRequest>> {
            Ok(self.merged_prs.lock().expect("poisoned")
                .iter()
                .find(|(name, _)| name == ref_name)
                .map(|(_, pr)| pr.clone()))
        }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<crate::forge::types::PrState> {
            Ok(crate::forge::types::PrState { merged: false, state: "open".to_string() })
        }
    }

    fn make_pr(name: &str, number: u64) -> PullRequest {
        PullRequest {
            number,
            html_url: format!("https://github.com/o/r/pull/{number}"),
            title: name.to_string(),
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
        }
    }

    fn make_segment(name: &str) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.to_string(),
                commit_id: format!("commit_{name}"),
                change_id: format!("change_{name}"),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![],
            merge_source_names: vec![],
        }
    }

    fn repo_info() -> RepoInfo {
        RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        }
    }

    fn default_options() -> MergeOptions {
        MergeOptions {
            merge_method: MergeMethod::Squash,
            required_approvals: 1,
            require_ci_pass: true,
            reconcile_strategy: crate::config::ReconcileStrategy::Merge,
            ready: false,
        }
    }

    fn default_plan() -> MergePlan {
        MergePlan {
            actions: vec![],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
        }
    }

    fn test_opts() -> WatchOptions {
        WatchOptions {
            shutdown: Arc::new(AtomicBool::new(false)),
            timeout: None,
            poll_interval: Duration::ZERO,
        }
    }

    #[test]
    fn test_watch_merges_immediately_when_ready() {
        let forge = ScriptedForge::new(vec![EvalResult::Mergeable])
            .with_prs(vec![make_pr("auth", 1)]);
        let segments = vec![make_segment("auth")];
        let plan = default_plan();

        let result = execute_merge_plan_watch(
            &StubJj, &forge, &plan, &segments, test_opts(),
        )
        .unwrap();

        assert_eq!(result.merged.len(), 1);
        assert_eq!(result.merged[0].pr_number, 1);
        assert_eq!(forge.merge_calls(), vec![1]);
    }

    #[test]
    fn test_watch_waits_then_merges() {
        let forge = ScriptedForge::new(vec![
            EvalResult::Blocked(vec![BlockReason::ChecksPending]),
            EvalResult::Mergeable,
        ])
        .with_prs(vec![make_pr("auth", 1)]);
        let segments = vec![make_segment("auth")];
        let plan = default_plan();

        let result = execute_merge_plan_watch(
            &StubJj, &forge, &plan, &segments, test_opts(),
        )
        .unwrap();

        assert_eq!(result.merged.len(), 1);
        assert_eq!(forge.merge_calls(), vec![1]);
    }

    #[test]
    fn test_watch_continues_across_segments() {
        let forge = ScriptedForge::new(vec![
            EvalResult::Mergeable,
            EvalResult::Blocked(vec![BlockReason::ChecksPending]),
            EvalResult::Mergeable,
        ])
        .with_prs(vec![make_pr("auth", 1), make_pr("profile", 2)]);
        let segments = vec![make_segment("auth"), make_segment("profile")];
        let plan = default_plan();

        let result = execute_merge_plan_watch(
            &StubJj, &forge, &plan, &segments, test_opts(),
        )
        .unwrap();

        assert_eq!(result.merged.len(), 2);
        assert_eq!(forge.merge_calls(), vec![1, 2]);
    }

    #[test]
    fn test_watch_stops_at_nopr() {
        let forge = ScriptedForge::new(vec![])
            .with_prs(vec![]);
        let segments = vec![make_segment("auth")];
        let plan = default_plan();

        let result = execute_merge_plan_watch(
            &StubJj, &forge, &plan, &segments, test_opts(),
        )
        .unwrap();

        assert!(result.merged.is_empty());
        assert!(result.blocked_at.is_some());
        let blocked = result.blocked_at.unwrap();
        assert!(blocked.reasons.iter().any(|r| matches!(r, BlockReason::NoPr)));
    }

    #[test]
    fn test_watch_respects_shutdown_flag() {
        let forge = ScriptedForge::new(vec![
            EvalResult::Blocked(vec![BlockReason::ChecksPending]),
        ])
        .with_prs(vec![make_pr("auth", 1)]);
        let segments = vec![make_segment("auth")];
        let plan = default_plan();

        let result = execute_merge_plan_watch(
            &StubJj, &forge, &plan, &segments,
            WatchOptions {
                shutdown: Arc::new(AtomicBool::new(true)),
                timeout: None,
                poll_interval: Duration::ZERO,
            },
        )
        .unwrap();

        assert!(result.merged.is_empty());
        assert!(forge.merge_calls().is_empty());
    }

    #[test]
    fn test_watch_respects_timeout() {
        let forge = ScriptedForge::new(vec![
            EvalResult::Blocked(vec![BlockReason::ChecksPending]),
        ])
        .with_prs(vec![make_pr("auth", 1)]);
        let segments = vec![make_segment("auth")];
        let plan = default_plan();

        let result = execute_merge_plan_watch(
            &StubJj, &forge, &plan, &segments,
            WatchOptions {
                shutdown: Arc::new(AtomicBool::new(false)),
                timeout: Some(Duration::ZERO),
                poll_interval: Duration::ZERO,
            },
        )
        .unwrap();

        assert!(result.merged.is_empty());
    }

    #[test]
    fn test_watch_gives_up_after_max_errors() {
        struct FailingForge;
        impl Forge for FailingForge {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![]) // No PRs — forces find_merged_pr call
            }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> {
                anyhow::bail!("API error")
            }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _ref_name: &str) -> Result<ChecksStatus> {
                Ok(ChecksStatus::Pass)
            }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> {
                Ok(ReviewSummary { approved_count: 1, changes_requested: false })
            }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { Ok(()) }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _body: &str, _h: &str, _b: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { Ok(()) }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { Ok(()) }
            fn list_comments(&self, _o: &str, _r: &str, _n: u64) -> Result<Vec<crate::forge::IssueComment>> { Ok(vec![]) }
            fn create_comment(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<crate::forge::IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { Ok(()) }
            fn get_authenticated_user(&self) -> Result<String> { Ok("user".to_string()) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _ref_name: &str) -> Result<Option<PullRequest>> {
                anyhow::bail!("API error")
            }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<crate::forge::types::PrState> {
                Ok(crate::forge::types::PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("auth")];
        let plan = default_plan();

        let result = execute_merge_plan_watch(
            &StubJj, &FailingForge, &plan, &segments, test_opts(),
        )
        .unwrap();

        assert!(result.merged.is_empty());
    }

    #[test]
    fn test_report_status_changes_first_time() {
        let reasons = vec![BlockReason::ChecksPending, BlockReason::Draft];
        let changed = report_status_changes("auth", None, &reasons, ForgeKind::GitHub);
        assert!(changed);
    }

    #[test]
    fn test_report_status_changes_no_change() {
        let reasons = vec![BlockReason::ChecksPending];
        let changed = report_status_changes("auth", Some(&reasons), &reasons, ForgeKind::GitHub);
        assert!(!changed);
    }

    #[test]
    fn test_report_status_changes_reason_resolved() {
        let prev = vec![BlockReason::ChecksPending, BlockReason::Draft];
        let current = vec![BlockReason::Draft];
        let changed = report_status_changes("auth", Some(&prev), &current, ForgeKind::GitHub);
        assert!(changed);
    }

    #[test]
    fn test_watch_reconciles_after_already_merged() {
        use std::sync::Mutex;

        struct RecordingJj {
            calls: Mutex<Vec<String>>,
        }
        impl RecordingJj {
            fn new() -> Self { Self { calls: Mutex::new(Vec::new()) } }
            fn calls(&self) -> Vec<String> { self.calls.lock().expect("poisoned").clone() }
        }
        impl Jj for RecordingJj {
            fn git_fetch(&self) -> Result<()> {
                self.calls.lock().expect("poisoned").push("git_fetch".to_string());
                Ok(())
            }
            fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<crate::jj::types::GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
            fn push_bookmark(&self, name: &str, _remote: &str) -> Result<()> {
                self.calls.lock().expect("poisoned").push(format!("push:{name}"));
                Ok(())
            }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
            fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { Ok(()) }
            fn merge_into(&self, bookmark: &str, dest: &str) -> Result<()> {
                self.calls.lock().expect("poisoned").push(format!("merge_into:{bookmark}:{dest}"));
                Ok(())
            }
            fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
                Ok(vec!["dummy".to_string()])
            }
            fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
        }

        // auth: not in open_prs, but find_merged_pr returns it → AlreadyMerged
        // profile: in open_prs, all checks pass → Mergeable
        let forge = ScriptedForge::new(vec![EvalResult::Mergeable])
            .with_prs(vec![make_pr("profile", 2)]);

        // Override find_merged_pr to return auth as merged
        *forge.merged_prs.lock().expect("poisoned") =
            vec![("auth".to_string(), make_pr("auth", 1))];

        let segments = vec![make_segment("auth"), make_segment("profile")];
        let jj = RecordingJj::new();

        let result = execute_merge_plan_watch(
            &jj, &forge, &default_plan(), &segments, test_opts(),
        )
        .unwrap();

        // auth skipped (already merged), profile merged
        assert_eq!(result.skipped_merged.len(), 1);
        assert_eq!(result.merged.len(), 1);

        // Reconciliation ran between segments — git_fetch is proof
        let jj_calls = jj.calls();
        assert!(
            jj_calls.iter().any(|c| c == "git_fetch"),
            "reconcile should have run after AlreadyMerged: {jj_calls:?}"
        );
    }
}
