use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::Result;

use crate::forge::types::{ChecksStatus, PullRequest, RepoInfo};
use crate::forge::{Forge, ForgeKind};
use crate::graph::change_graph;
use crate::jj::types::NarrowedSegment;
use crate::jj::Jj;
use crate::merge::execute::{
    format_block_reason, merge_with_retry, reconcile_after_merge, BlockedPr,
    LocalDivergenceWarning, MergeResult, MergedPr, SkippedMergedPr,
};
use crate::merge::plan::{evaluate_segment, BlockReason, MergeOptions, PrMergeStatus};
use crate::merge::watch::{
    interruptible_sleep, local_time_hhmm, refresh_pr_map, report_status_changes,
    WatchOptions, HEARTBEAT_INTERVAL, MAX_CONSECUTIVE_ERRORS,
};
use crate::submit::{analyze, plan, execute, resolve};

#[derive(Debug)]
pub struct CreatedPr {
    pub bookmark_name: String,
    pub pr_number: u64,
}

#[derive(Debug)]
pub struct PromotedPr {
    pub bookmark_name: String,
    pub pr_number: u64,
}

#[derive(Debug)]
pub struct WatchResult {
    pub prs_created: Vec<CreatedPr>,
    pub prs_promoted: Vec<PromotedPr>,
    pub merge_result: MergeResult,
}

/// Promote draft PRs to ready when their CI checks pass.
fn promote_ready_drafts(
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    pr_map: &HashMap<String, PullRequest>,
    repo_info: &RepoInfo,
    fk: ForgeKind,
) -> Vec<PromotedPr> {
    let mut promoted = Vec::new();
    let owner = &repo_info.owner;
    let repo = &repo_info.repo;

    for seg in segments {
        let Some(pr) = pr_map.get(&seg.bookmark.name) else {
            continue;
        };
        if !pr.draft {
            continue;
        }

        let checks_ref = if pr.head.sha.is_empty() {
            &pr.head.ref_name
        } else {
            &pr.head.sha
        };

        let Ok(status) = forge.get_pr_checks_status(owner, repo, checks_ref) else {
            continue;
        };

        if status == ChecksStatus::Pass {
            if let Err(e) = forge.mark_pr_ready(owner, repo, pr.number) {
                eprintln!(
                    "  Warning: failed to mark {} as ready: {e}",
                    fk.format_ref(pr.number)
                );
                continue;
            }
            println!(
                "  Marked '{}' as ready (CI passing)",
                seg.bookmark.name
            );
            promoted.push(PromotedPr {
                bookmark_name: seg.bookmark.name.clone(),
                pr_number: pr.number,
            });
        }
    }

    promoted
}

/// Check if a blocked PR needs a reviewer hint and return the hint text if so.
fn reviewer_hint(
    pr: Option<&PullRequest>,
    reasons: &[BlockReason],
    bookmark_name: &str,
    fk: ForgeKind,
) -> Option<String> {
    let pr = pr?;
    if !reasons.iter().any(|r| matches!(r, BlockReason::InsufficientApprovals { .. })) {
        return None;
    }
    if !pr.requested_reviewers.is_empty() {
        return None;
    }
    Some(format!(
        "\n  '{}' ({}): needs review approval but has no reviewers\n\
         \x20   hint: run `jjpr submit --reviewer <username>` to request reviewers",
        bookmark_name,
        fk.format_ref(pr.number),
    ))
}

/// Build a MergePlan-like context for reconcile_after_merge calls.
fn make_merge_plan(
    repo_info: &RepoInfo,
    forge_kind: ForgeKind,
    default_branch: &str,
    remote_name: &str,
    options: &MergeOptions,
    stack_base: Option<&str>,
    stack_nav: crate::config::StackNavMode,
) -> crate::merge::plan::MergePlan {
    crate::merge::plan::MergePlan {
        actions: vec![],
        repo_info: repo_info.clone(),
        forge_kind,
        default_branch: default_branch.to_string(),
        remote_name: remote_name.to_string(),
        options: options.clone(),
        stack_base: stack_base.map(|s| s.to_string()),
        stack_nav,
    }
}

struct MergePhaseOutcome {
    merged: Vec<MergedPr>,
    skipped: Vec<SkippedMergedPr>,
    blocked: Option<BlockedPr>,
    all_done: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_merge_phase(
    jj: &dyn Jj,
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    pr_map: &HashMap<String, PullRequest>,
    merge_options: &MergeOptions,
    merge_plan: &crate::merge::plan::MergePlan,
    forge_kind: ForgeKind,
    prev_reasons: &mut Option<Vec<BlockReason>>,
    consecutive_errors: &mut u32,
    last_heartbeat: &mut Instant,
    local_degraded: &mut bool,
    local_warnings: &mut Vec<LocalDivergenceWarning>,
) -> Result<MergePhaseOutcome> {
    let owner = &merge_plan.repo_info.owner;
    let repo = &merge_plan.repo_info.repo;
    let mut pr_map = pr_map.clone();
    let mut merged = Vec::new();
    let mut skipped = Vec::new();
    let mut seg_idx = 0;
    let mut advanced = false;

    while seg_idx < segments.len() {
        let segment = &segments[seg_idx];
        let status = match evaluate_segment(
            forge,
            &segment.bookmark.name,
            &merge_plan.repo_info,
            &pr_map,
            merge_options,
        ) {
            Ok(s) => s,
            Err(e) => {
                *consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!("  [{now}] Eval error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                break;
            }
        };
        *consecutive_errors = 0;

        let prev_seg_idx = seg_idx;

        match status {
            PrMergeStatus::AlreadyMerged {
                bookmark_name,
                pr_number,
            } => {
                if prev_reasons.is_some() {
                    println!(
                        "  {bookmark_name}: Merged externally ({}) \u{2014} moving on",
                        forge_kind.format_ref(pr_number)
                    );
                } else {
                    println!(
                        "  '{bookmark_name}' ({}) already merged",
                        forge_kind.format_ref(pr_number)
                    );
                }
                skipped.push(SkippedMergedPr {
                    bookmark_name,
                    pr_number,
                });
                *prev_reasons = None;
                seg_idx += 1;
                advanced = true;
            }

            PrMergeStatus::Mergeable { bookmark_name, pr } => {
                if prev_reasons.is_some() {
                    println!("  {bookmark_name}: Ready to merge");
                }

                println!(
                    "\n  Merging '{bookmark_name}' ({}, {})...",
                    forge_kind.format_ref(pr.number),
                    merge_options.merge_method
                );
                println!("    {}", pr.html_url);

                merge_with_retry(
                    forge,
                    owner,
                    repo,
                    pr.number,
                    merge_options.merge_method,
                    forge_kind,
                )?;

                merged.push(MergedPr {
                    bookmark_name,
                    pr_number: pr.number,
                    html_url: pr.html_url.clone(),
                });

                *prev_reasons = None;
                seg_idx += 1;
                advanced = true;
            }

            PrMergeStatus::Blocked {
                bookmark_name,
                pr,
                reasons,
            } => {
                if reasons.iter().any(|r| matches!(r, BlockReason::NoPr)) {
                    return Ok(MergePhaseOutcome {
                        merged,
                        skipped,
                        blocked: Some(BlockedPr {
                            bookmark_name,
                            pr_number: None,
                            reasons,
                        }),
                        all_done: false,
                    });
                }

                if prev_reasons.is_none()
                    && let Some(hint) = reviewer_hint(pr.as_ref(), &reasons, &bookmark_name, forge_kind)
                {
                    println!("{hint}");
                }

                let changed = report_status_changes(
                    &bookmark_name,
                    prev_reasons.as_deref(),
                    &reasons,
                    forge_kind,
                );

                if !changed && last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                    let now = local_time_hhmm();
                    let first_reason = reasons
                        .first()
                        .map(|r| format_block_reason(r, forge_kind))
                        .unwrap_or_default();
                    println!(
                        "  [{now}] Still waiting for {bookmark_name}: {first_reason}"
                    );
                    *last_heartbeat = Instant::now();
                }

                if changed {
                    *last_heartbeat = Instant::now();
                }

                *prev_reasons = Some(reasons);
                break; // Wait for next iteration
            }
        }

        // Reconcile after advancing
        if seg_idx > prev_seg_idx && seg_idx < segments.len() {
            let fresh = reconcile_after_merge(
                jj,
                forge,
                segments,
                prev_seg_idx,
                merge_plan,
                forge_kind,
                local_degraded,
                local_warnings,
            );
            if let Some(fresh_map) = fresh {
                pr_map = fresh_map;
            }
        }
    }

    Ok(MergePhaseOutcome {
        merged,
        skipped,
        blocked: None,
        all_done: seg_idx >= segments.len() && advanced,
    })
}

/// Run the watch loop: submit → promote → merge → repeat.
#[allow(clippy::too_many_arguments)]
pub fn run_watch_loop(
    jj: &dyn Jj,
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    forge_kind: ForgeKind,
    remote_name: &str,
    default_branch: &str,
    merge_options: &MergeOptions,
    target_bookmark: &str,
    stack_base: Option<&str>,
    stack_nav: crate::config::StackNavMode,
    opts: WatchOptions,
) -> Result<WatchResult> {
    let shutdown = opts.shutdown;
    let timeout = opts.timeout;
    let poll_interval = opts.poll_interval;
    let owner = &repo_info.owner;
    let repo = &repo_info.repo;

    let mut all_created: Vec<CreatedPr> = Vec::new();
    let mut all_promoted: Vec<PromotedPr> = Vec::new();
    let mut merged: Vec<MergedPr> = Vec::new();
    let mut blocked_at: Option<BlockedPr> = None;
    let mut skipped_merged: Vec<SkippedMergedPr> = Vec::new();
    let mut local_warnings: Vec<LocalDivergenceWarning> = Vec::new();
    let mut local_degraded = false;

    let mut prev_reasons: Option<Vec<BlockReason>> = None;
    let mut consecutive_errors: u32 = 0;
    let mut last_heartbeat = Instant::now();
    let deadline = timeout.map(|d| Instant::now() + d);

    let merge_plan = make_merge_plan(
        repo_info, forge_kind, default_branch, remote_name, merge_options, stack_base, stack_nav,
    );

    // Print initial status so the user knows what watch is working with
    if let Ok(initial_prs) = forge.list_open_prs(owner, repo) {
        let pr_map = crate::forge::build_pr_map(initial_prs, owner);
        let segments = rediscover_segments(jj, target_bookmark).unwrap_or_default();
        let with_pr: Vec<_> = segments.iter()
            .filter(|s| pr_map.contains_key(&s.bookmark.name))
            .collect();
        let without_pr: Vec<_> = segments.iter()
            .filter(|s| !pr_map.contains_key(&s.bookmark.name))
            .collect();
        if !with_pr.is_empty() || !without_pr.is_empty() {
            println!("  {} bookmark{} in stack{}",
                segments.len(),
                if segments.len() == 1 { "" } else { "s" },
                if !with_pr.is_empty() {
                    format!(", {} with existing PR{}",
                        with_pr.len(),
                        if with_pr.len() == 1 { "" } else { "s" })
                } else {
                    String::new()
                },
            );
            if !without_pr.is_empty() {
                let names: Vec<_> = without_pr.iter().map(|s| s.bookmark.name.as_str()).collect();
                println!("  Will create draft PRs for: {}\n", names.join(", "));
            } else {
                println!();
            }
        }
    }

    if merge_options.required_approvals == 0 {
        anyhow::bail!(
            "jjpr watch requires at least 1 approval to merge (required_approvals is 0).\n\
             \n\
             With 0 required approvals, watch would auto-merge PRs the moment CI \n\
             passes — no human review. Set required_approvals = 1 in your config \n\
             or pass --required-approvals 1.\n\
             \n\
             If you need to merge without approvals, use `jjpr merge` instead."
        );
    }

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            println!("\nWatch timed out.");
            break;
        }

        // --- Phase 1: Re-discover segments ---
        let segments = match rediscover_segments(jj, target_bookmark) {
            Ok(segs) => segs,
            Err(e) => {
                consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!("  [{now}] Graph scan error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("  Too many consecutive errors \u{2014} giving up.");
                    break;
                }
                if interruptible_sleep(poll_interval, &shutdown) {
                    break;
                }
                continue;
            }
        };

        if segments.is_empty() {
            break;
        }

        // --- Phase 1b: Check for conflicts ---
        let has_conflicts = segments.iter().any(|seg|
            seg.changes.iter().any(|c| c.conflict)
        );
        if has_conflicts {
            if prev_reasons.is_none() {
                let conflicted: Vec<_> = segments.iter()
                    .flat_map(|seg| seg.changes.iter().filter(|c| c.conflict)
                        .map(|c| (seg.bookmark.name.as_str(), c.change_id.as_str())))
                    .collect();
                println!("\n  Waiting for conflict resolution:");
                for (bookmark, change_id) in &conflicted {
                    println!("    - {change_id} ({bookmark})");
                }
                println!("    hint: jj edit <change_id>, fix the conflicts, then jjpr watch will continue");
            }
            if interruptible_sleep(poll_interval, &shutdown) {
                break;
            }
            continue;
        }

        // --- Phase 2: Submit (push + create draft PRs) ---
        let bookmarks_being_created = match run_submit_phase(jj, forge, &segments, remote_name, repo_info, forge_kind, default_branch, stack_base, stack_nav) {
            Ok(names) => {
                consecutive_errors = 0;
                names
            }
            Err(e) => {
                consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!("  [{now}] Submit error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    break;
                }
                if interruptible_sleep(poll_interval, &shutdown) {
                    break;
                }
                continue;
            }
        };

        // --- Phase 3: Refresh PR map ---
        let pr_map = match refresh_pr_map(forge, owner, repo) {
            Ok(m) => {
                consecutive_errors = 0;
                m
            }
            Err(e) => {
                consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!("  [{now}] PR refresh error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    break;
                }
                if interruptible_sleep(poll_interval, &shutdown) {
                    break;
                }
                continue;
            }
        };

        // Resolve created PRs from the fresh PR map (avoids an extra API call)
        for name in &bookmarks_being_created {
            if let Some(pr) = pr_map.get(name) {
                println!("    {}", forge_kind.format_ref(pr.number));
                all_created.push(CreatedPr {
                    bookmark_name: name.clone(),
                    pr_number: pr.number,
                });
            }
        }

        // --- Phase 4: Promote draft PRs with passing CI ---
        let promoted = promote_ready_drafts(forge, &segments, &pr_map, repo_info, forge_kind);

        // Refresh PR map after promotions so evaluate_segment sees updated draft status
        let pr_map = if !promoted.is_empty() {
            refresh_pr_map(forge, owner, repo).unwrap_or(pr_map)
        } else {
            pr_map
        };
        all_promoted.extend(promoted);

        // --- Phase 5: Merge phase (bottom-up) ---
        let merge_outcome = run_merge_phase(
            jj, forge, &segments, &pr_map, merge_options, &merge_plan,
            forge_kind, &mut prev_reasons, &mut consecutive_errors,
            &mut last_heartbeat, &mut local_degraded, &mut local_warnings,
        )?;

        merged.extend(merge_outcome.merged);
        skipped_merged.extend(merge_outcome.skipped);

        if let Some(blocked) = merge_outcome.blocked {
            blocked_at = Some(blocked);
            break;
        }
        if merge_outcome.all_done {
            break;
        }

        // Sleep before next iteration
        if interruptible_sleep(poll_interval, &shutdown) {
            break;
        }
    }

    Ok(WatchResult {
        prs_created: all_created,
        prs_promoted: all_promoted,
        merge_result: MergeResult {
            merged,
            blocked_at,
            skipped_merged,
            local_warnings,
        },
    })
}

/// Re-discover segments by rebuilding the change graph.
fn rediscover_segments(
    jj: &dyn Jj,
    target_bookmark: &str,
) -> Result<Vec<NarrowedSegment>> {
    let graph = change_graph::build_change_graph(jj)?;

    // If the target bookmark no longer exists (fully merged), return empty
    let analysis = match analyze::analyze_submission_graph(&graph, target_bookmark) {
        Ok(a) => a,
        Err(_) => return Ok(vec![]),
    };

    resolve::resolve_bookmark_selections(&analysis.relevant_segments, false)
}

/// Run the submit phase: push unsynced bookmarks, create draft PRs, update bases/bodies.
///
/// Returns the names of bookmarks that had new PRs created. The caller resolves
/// PR numbers from the PR map (which is refreshed immediately after this phase),
/// avoiding an extra list_open_prs API call.
fn run_submit_phase(
    jj: &dyn Jj,
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    remote_name: &str,
    repo_info: &RepoInfo,
    forge_kind: ForgeKind,
    default_branch: &str,
    stack_base: Option<&str>,
    stack_nav: crate::config::StackNavMode,
) -> Result<Vec<String>> {
    let submission_plan = plan::create_submission_plan(
        forge,
        segments,
        remote_name,
        repo_info,
        forge_kind,
        default_branch,
        &plan::SubmitOptions {
            draft: true,
            ready: false,
            reviewers: &[],
            stack_base,
            stack_nav,
        },
    )?;

    if !submission_plan.has_actions() {
        return Ok(vec![]);
    }

    let creating: Vec<String> = submission_plan
        .bookmarks_needing_pr
        .iter()
        .map(|b| b.bookmark.name.clone())
        .collect();

    execute::execute_submission_plan(jj, forge, &submission_plan, &[], false)?;

    Ok(creating)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::forge::types::{
        ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequest,
        PullRequestRef, ReviewSummary,
    };
    use crate::jj::types::Bookmark;

    // --- Test helpers ---

    fn make_pr(name: &str, number: u64, draft: bool) -> PullRequest {
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
            draft,
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

    // --- Forge stub for promotion tests ---

    struct PromotionForge {
        calls: Mutex<Vec<String>>,
        prs: HashMap<String, PullRequest>,
        checks: HashMap<String, ChecksStatus>,
    }

    impl PromotionForge {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                prs: HashMap::new(),
                checks: HashMap::new(),
            }
        }

        fn with_pr(mut self, pr: PullRequest, checks: ChecksStatus) -> Self {
            let sha_key = if pr.head.sha.is_empty() {
                pr.head.ref_name.clone()
            } else {
                pr.head.sha.clone()
            };
            self.checks.insert(sha_key, checks);
            self.prs.insert(pr.head.ref_name.clone(), pr);
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("poisoned").clone()
        }
    }

    impl Forge for PromotionForge {
        fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
            Ok(self.prs.values().cloned().collect())
        }
        fn get_pr_checks_status(&self, _o: &str, _r: &str, ref_name: &str) -> Result<ChecksStatus> {
            self.checks.get(ref_name).cloned()
                .ok_or_else(|| anyhow::anyhow!("no checks for {ref_name}"))
        }
        fn mark_pr_ready(&self, _o: &str, _r: &str, number: u64) -> Result<()> {
            self.calls.lock().expect("poisoned")
                .push(format!("mark_pr_ready:{number}"));
            Ok(())
        }
        fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
        fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
        fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { Ok(()) }
        fn list_comments(&self, _o: &str, _r: &str, _n: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
        fn create_comment(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
        fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { Ok(()) }
        fn get_authenticated_user(&self) -> Result<String> { Ok("user".to_string()) }
        fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { Ok(None) }
        fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { Ok(()) }
        fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary { approved_count: 0, changes_requested: false })
        }
        fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> {
            Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".to_string() })
        }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
            Ok(PrState { merged: false, state: "open".to_string() })
        }
    }

    // --- Reviewer hint tests ---

    #[test]
    fn test_reviewer_hint_shown_when_no_reviewers() {
        let pr = make_pr("auth", 42, false);
        let reasons = vec![BlockReason::InsufficientApprovals { have: 0, need: 1 }];

        let hint = reviewer_hint(Some(&pr), &reasons, "auth", ForgeKind::GitHub);

        assert!(hint.is_some(), "should show hint when no reviewers");
        let text = hint.unwrap();
        assert!(text.contains("no reviewers"), "hint text: {text}");
        assert!(text.contains("jjpr submit --reviewer"), "hint text: {text}");
    }

    #[test]
    fn test_reviewer_hint_not_shown_when_reviewers_present() {
        let mut pr = make_pr("auth", 42, false);
        pr.requested_reviewers = vec!["alice".to_string()];
        let reasons = vec![BlockReason::InsufficientApprovals { have: 0, need: 1 }];

        let hint = reviewer_hint(Some(&pr), &reasons, "auth", ForgeKind::GitHub);

        assert!(hint.is_none(), "should not show hint when reviewers are present");
    }

    #[test]
    fn test_reviewer_hint_not_shown_for_non_approval_blocks() {
        let pr = make_pr("auth", 42, false);
        let reasons = vec![BlockReason::ChecksPending];

        let hint = reviewer_hint(Some(&pr), &reasons, "auth", ForgeKind::GitHub);

        assert!(hint.is_none(), "should not show hint for non-approval blocks");
    }

    #[test]
    fn test_reviewer_hint_not_shown_when_no_pr() {
        let reasons = vec![BlockReason::NoPr];

        let hint = reviewer_hint(None, &reasons, "auth", ForgeKind::GitHub);

        assert!(hint.is_none(), "should not show hint when there's no PR");
    }

    // --- Promotion tests ---

    #[test]
    fn test_promote_draft_when_ci_passes() {
        let forge = PromotionForge::new()
            .with_pr(make_pr("auth", 1, true), ChecksStatus::Pass);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted = promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].pr_number, 1);
        assert!(forge.calls().contains(&"mark_pr_ready:1".to_string()));
    }

    #[test]
    fn test_no_promote_when_ci_pending() {
        let forge = PromotionForge::new()
            .with_pr(make_pr("auth", 1, true), ChecksStatus::Pending);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted = promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty());
        assert!(!forge.calls().iter().any(|c| c.starts_with("mark_pr_ready")));
    }

    #[test]
    fn test_no_promote_when_ci_failing() {
        let forge = PromotionForge::new()
            .with_pr(make_pr("auth", 1, true), ChecksStatus::Fail);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted = promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty());
    }

    #[test]
    fn test_no_promote_when_not_draft() {
        let forge = PromotionForge::new()
            .with_pr(make_pr("auth", 1, false), ChecksStatus::Pass);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted = promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty());
    }

    #[test]
    fn test_no_promote_when_no_ci_checks() {
        let forge = PromotionForge::new()
            .with_pr(make_pr("auth", 1, true), ChecksStatus::None);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted = promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty(), "should not promote when no CI checks exist");
    }

    #[test]
    fn test_promote_multiple_drafts_in_stack() {
        let forge = PromotionForge::new()
            .with_pr(make_pr("auth", 1, true), ChecksStatus::Pass)
            .with_pr(make_pr("profile", 2, true), ChecksStatus::Pass)
            .with_pr(make_pr("settings", 3, true), ChecksStatus::Pass);
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted = promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert_eq!(promoted.len(), 3);
        let calls = forge.calls();
        assert!(calls.contains(&"mark_pr_ready:1".to_string()));
        assert!(calls.contains(&"mark_pr_ready:2".to_string()));
        assert!(calls.contains(&"mark_pr_ready:3".to_string()));
    }
}
