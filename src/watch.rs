use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::forge::types::{ChecksStatus, PullRequest, RepoInfo};
use crate::forge::{Forge, ForgeKind};
use crate::graph::change_graph;
use crate::jj::types::NarrowedSegment;
use crate::jj::Jj;
use crate::merge::execute::{
    format_block_reason, merge_with_retry, rebase_root, reconcile_after_merge, BlockedPr,
    DivergenceKind, MergeResult, MergedPr, ReconcileState, SkippedMergedPr,
};
use crate::merge::plan::{evaluate_segment, BlockReason, MergeOptions, PrMergeStatus};
use crate::merge::watch::{
    clear_dot_line, interruptible_sleep, local_time_hhmm, refresh_pr_map,
    report_status_changes, WatchOptions, HEARTBEAT_INTERVAL, MAX_CONSECUTIVE_ERRORS,
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

/// What the outer watch loop should do after a merge phase, given the
/// reconcile state and whether we were already in a degraded state.
/// Pure function of inputs; the loop dispatches the side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostMergeAction {
    /// State is clean and we weren't previously degraded: proceed with
    /// no-progress checks, all_done handling, and the normal sleep.
    Continue,
    /// State just recovered from a previously-reported failure. Print the
    /// recovery message, clear `prev_reconcile_block`, then proceed as
    /// `Continue` would.
    Recovered,
    /// State is degraded with a different set of reasons than last time
    /// (or the first time we hit this in the session). Print the full
    /// recovery hints, then sleep and retry.
    NewFailure,
    /// State is degraded with the same reasons as last time, and the
    /// heartbeat interval has elapsed. Print a one-line heartbeat, then
    /// sleep and retry.
    Heartbeat,
    /// State is degraded with the same reasons as last time, before the
    /// heartbeat interval. Print a dot and sleep and retry.
    Dot,
}

impl PostMergeAction {
    /// Whether the loop should sleep and `continue` after handling this
    /// action. True for any degraded action; false for clean ones.
    fn waits(self) -> bool {
        matches!(self, Self::NewFailure | Self::Heartbeat | Self::Dot)
    }
}

fn classify_post_merge(
    state: &ReconcileState,
    prev_reconcile_block: &Option<Vec<BlockReason>>,
    last_heartbeat_elapsed: Duration,
    heartbeat_interval: Duration,
) -> PostMergeAction {
    if !state.degraded() {
        return if prev_reconcile_block.is_some() {
            PostMergeAction::Recovered
        } else {
            PostMergeAction::Continue
        };
    }
    let current = state.block_reasons();
    if prev_reconcile_block.as_ref() != Some(&current) {
        PostMergeAction::NewFailure
    } else if last_heartbeat_elapsed >= heartbeat_interval {
        PostMergeAction::Heartbeat
    } else {
        PostMergeAction::Dot
    }
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
    state: &mut ReconcileState,
    dots_on_line: &mut bool,
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
                clear_dot_line(dots_on_line);
                if prev_reasons.is_some() {
                    println!(
                        "  {bookmark_name}: Merged externally ({}); moving on",
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
                clear_dot_line(dots_on_line);
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
                    clear_dot_line(dots_on_line);
                    // Match execute_merge_plan's UX: name the bookmark so
                    // the user knows where the stack stopped.
                    println!("\n  Blocked at '{bookmark_name}':");
                    println!("    - {}", format_block_reason(&BlockReason::NoPr, forge_kind));
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
                    clear_dot_line(dots_on_line);
                    println!("{hint}");
                }

                match report_status_changes(
                    &bookmark_name,
                    prev_reasons.as_deref(),
                    &reasons,
                    forge_kind,
                ) {
                    Some(displayed) => {
                        clear_dot_line(dots_on_line);
                        *prev_reasons = Some(displayed);
                        *last_heartbeat = Instant::now();
                    }
                    None => {
                        if prev_reasons.is_none() {
                            *prev_reasons = Some(vec![]);
                        }
                        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                            clear_dot_line(dots_on_line);
                            let now = local_time_hhmm();
                            let first_reason = reasons
                                .first()
                                .map(|r| format_block_reason(r, forge_kind))
                                .unwrap_or_default();
                            println!(
                                "  [{now}] Still waiting for {bookmark_name}: {first_reason}"
                            );
                            *last_heartbeat = Instant::now();
                        } else {
                            print!(".");
                            let _ = std::io::stdout().flush();
                            *dots_on_line = true;
                        }
                    }
                }
                break; // Wait for next iteration
            }
        }

        // Reconcile after advancing
        if seg_idx > prev_seg_idx && seg_idx < segments.len() {
            let fresh = reconcile_after_merge(
                jj, forge, segments, prev_seg_idx, merge_plan, forge_kind, state,
            );
            if let Some(fresh_map) = fresh {
                pr_map = fresh_map;
            }

            // Stop the merge phase if reconcile produced any failures.
            // The outer watch loop reads `state.degraded()` and decides
            // whether to print recovery hints, sleep, and retry. Watch
            // is persistent: when the user fixes local state, the next
            // iteration's reconcile gets a fresh chance and we resume.
            if state.degraded() {
                clear_dot_line(dots_on_line);
                break;
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
/// Poll until a bookmark appears in the working copy's ancestry.
///
/// Prints a "Waiting for a bookmark..." preamble, then rebuilds the change
/// graph every `poll_interval` and runs `infer_target_bookmark`. Returns
/// `Ok(Some(name))` as soon as one is found, or `Ok(None)` if `shutdown` is
/// set or `timeout` elapses. The caller decides what to print on the `None`
/// path so it can distinguish interrupt from timeout.
pub fn wait_for_bookmark(
    jj: &dyn Jj,
    timeout: Option<Duration>,
    poll_interval: Duration,
    shutdown: &AtomicBool,
) -> Result<Option<String>> {
    let deadline = timeout.map(|d| Instant::now() + d);

    println!("Waiting for a bookmark in the working copy's ancestry...");
    println!("    hint: jj bookmark set <name>\n");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            return Ok(None);
        }

        if let Ok(graph) = change_graph::build_change_graph(jj)
            && let Ok(Some(name)) = analyze::infer_target_bookmark(&graph, jj)
        {
            return Ok(Some(name));
        }

        if interruptible_sleep(poll_interval, shutdown) {
            return Ok(None);
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
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

    // Reconcile state is reset at the top of each outer-loop iteration so
    // a transient sync failure on iteration N doesn't disable sync on
    // iteration N+1. This is what lets watch resume cleanly after the
    // user fixes local state.
    let mut state = ReconcileState::default();
    let mut prev_reconcile_block: Option<Vec<BlockReason>> = None;

    // prev_reasons persists across outer iterations on purpose: report_status_changes
    // suppresses reprints when the same segment is still blocked on the same
    // reasons. The 60s heartbeat keeps the user informed by name+reason even
    // when the first-time header doesn't reprint, so the leak across iterations
    // (same reasons surfacing on a different segment) only delays a fresh
    // header by at most one heartbeat. Resetting it would cause "Waiting for
    // X: ..." to reprint every poll, which is what the deduplication exists
    // to prevent in the first place.
    let mut prev_reasons: Option<Vec<BlockReason>> = None;
    let mut consecutive_errors: u32 = 0;
    let mut last_heartbeat = Instant::now();
    let mut dots_on_line = false;
    let mut no_progress_count: u32 = 0;
    let deadline = timeout.map(|d| Instant::now() + d);

    let merge_plan = make_merge_plan(
        repo_info, forge_kind, default_branch, remote_name, merge_options, stack_base, stack_nav,
    );

    print_initial_watch_status(jj, forge, owner, repo, target_bookmark);

    if merge_options.required_approvals == 0 {
        anyhow::bail!(
            "jjpr watch requires at least 1 approval to merge (required_approvals is 0).\n\
             \n\
             With 0 required approvals, watch would auto-merge PRs the moment CI \n\
             passes, with no human review. Set required_approvals = 1 in your config \n\
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

        // Reset reconcile state at the top of each iteration. If the user
        // fixed local divergence since the last failure, this is what gives
        // reconcile_after_merge a fresh chance.
        state.reset();

        // --- Phase 1: Re-discover segments ---
        let segments = match rediscover_segments(jj, target_bookmark) {
            Ok(segs) => {
                consecutive_errors = 0;
                segs
            }
            Err(e) => {
                consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!("  [{now}] Graph scan error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("  Too many consecutive errors; giving up.");
                    break;
                }
                if interruptible_sleep(poll_interval, &shutdown) {
                    break;
                }
                continue;
            }
        };

        if segments.is_empty() {
            clear_dot_line(&mut dots_on_line);
            report_orphaned_prs(jj, forge, owner, repo, &merged, &skipped_merged, forge_kind);
            break;
        }

        // --- Phase 1b: Check for conflicts ---
        //
        // This and the submit/refresh error paths below `continue` before
        // reaching the classify_post_merge dispatch. That means: if the
        // previous iteration was reconcile-degraded, the "Local sync
        // recovered. Resuming." message is deferred until a later iteration
        // makes it all the way through the merge phase. Intentional: a
        // "recovered" announcement next to a fresh "waiting for conflict
        // resolution" would be confusing. prev_reconcile_block stays Some
        // and the eventual recovery still announces.
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
        let had_creates = !bookmarks_being_created.is_empty();
        let had_promotes = !promoted.is_empty();
        all_promoted.extend(promoted);

        // --- Phase 5: Merge phase (bottom-up) ---
        let merge_outcome = run_merge_phase(
            jj, forge, &segments, &pr_map, merge_options, &merge_plan,
            forge_kind, &mut prev_reasons, &mut consecutive_errors,
            &mut last_heartbeat, &mut state, &mut dots_on_line,
        )?;

        let total_before = merged.len() + skipped_merged.len();
        merged.extend(merge_outcome.merged);
        // Dedup skipped to avoid re-counting the same AlreadyMerged bookmark
        for s in merge_outcome.skipped {
            if !skipped_merged.iter().any(|existing| existing.bookmark_name == s.bookmark_name) {
                skipped_merged.push(s);
            }
        }
        let total_after = merged.len() + skipped_merged.len();
        let created_or_promoted = had_creates || had_promotes;

        if let Some(blocked) = merge_outcome.blocked {
            blocked_at = Some(blocked);
            break;
        }

        // Classify the post-merge state and dispatch. The classification
        // is pure (see classify_post_merge); the side effects live here.
        let action = classify_post_merge(
            &state, &prev_reconcile_block,
            last_heartbeat.elapsed(), HEARTBEAT_INTERVAL,
        );
        match action {
            PostMergeAction::Continue => {}
            PostMergeAction::Recovered => {
                clear_dot_line(&mut dots_on_line);
                println!("  Local sync recovered. Resuming.");
                prev_reconcile_block = None;
            }
            PostMergeAction::NewFailure => {
                clear_dot_line(&mut dots_on_line);
                report_reconcile_failure(&state, &segments, &merged, &skipped_merged,
                    stack_base, default_branch, forge_kind);
                prev_reconcile_block = Some(state.block_reasons());
                last_heartbeat = Instant::now();
            }
            PostMergeAction::Heartbeat => {
                clear_dot_line(&mut dots_on_line);
                let now = local_time_hhmm();
                println!("  [{now}] Still waiting for local sync to recover");
                last_heartbeat = Instant::now();
            }
            PostMergeAction::Dot => {
                print!(".");
                let _ = std::io::stdout().flush();
                dots_on_line = true;
            }
        }
        if action.waits() {
            if interruptible_sleep(poll_interval, &shutdown) {
                break;
            }
            continue;
        }

        // No-progress safety valve: must run even when all_done fires, because
        // rediscover_segments might keep returning the same already-merged segments.
        if total_after == total_before && !created_or_promoted {
            no_progress_count += 1;
            if no_progress_count >= 5 {
                clear_dot_line(&mut dots_on_line);
                println!("\n  No progress after {no_progress_count} consecutive iterations; exiting.");
                println!("  Remaining bookmarks may need manual intervention.");
                break;
            }
        } else {
            no_progress_count = 0;
        }

        if merge_outcome.all_done {
            // All segments in this snapshot were processed. Loop back to
            // rediscover — reconciliation after merge may have changed the
            // graph. Skip the sleep since we just made progress.
            continue;
        }

        // Sleep before next iteration
        if interruptible_sleep(poll_interval, &shutdown) {
            break;
        }
    }

    // local_warnings reflects only the LAST iteration's warnings, because
    // state.reset() at the top of each iteration wipes earlier ones. Earlier
    // failures were already announced inline by report_reconcile_failure;
    // the summary should not double-print them. If an exit condition fires
    // outside of report_reconcile_failure (timeout, shutdown, no_progress)
    // and state is currently degraded, those warnings surface in the summary.
    Ok(WatchResult {
        prs_created: all_created,
        prs_promoted: all_promoted,
        merge_result: MergeResult {
            merged,
            blocked_at,
            skipped_merged,
            local_warnings: state.warnings,
        },
    })
}

/// One-time pre-loop summary so the user knows what watch is working
/// with. Best-effort: silently skip if the forge or jj queries fail.
fn print_initial_watch_status(
    jj: &dyn Jj,
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
    target_bookmark: &str,
) {
    let Ok(initial_prs) = forge.list_open_prs(owner, repo) else {
        return;
    };
    let pr_map = crate::forge::build_pr_map(initial_prs, owner);
    let segments = rediscover_segments(jj, target_bookmark).unwrap_or_default();
    let with_pr: Vec<_> = segments.iter()
        .filter(|s| pr_map.contains_key(&s.bookmark.name))
        .collect();
    let without_pr: Vec<_> = segments.iter()
        .filter(|s| !pr_map.contains_key(&s.bookmark.name))
        .collect();
    if with_pr.is_empty() && without_pr.is_empty() {
        return;
    }
    let plural = if segments.len() == 1 { "" } else { "s" };
    let with_pr_suffix = if !with_pr.is_empty() {
        format!(", {} with existing PR{}",
            with_pr.len(), if with_pr.len() == 1 { "" } else { "s" })
    } else {
        String::new()
    };
    println!("  {} bookmark{plural} in stack{with_pr_suffix}", segments.len());
    if !without_pr.is_empty() {
        let names: Vec<_> = without_pr.iter().map(|s| s.bookmark.name.as_str()).collect();
        println!("  Will create draft PRs for: {}\n", names.join(", "));
    } else {
        println!();
    }
}

/// When the change graph has no segments but we have local bookmarks
/// pointing at open PRs we never processed, name them so the user knows
/// what's still in flight. Called only when watch is exiting because
/// the target bookmark vanished from the graph.
fn report_orphaned_prs(
    jj: &dyn Jj,
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
    merged: &[MergedPr],
    skipped: &[SkippedMergedPr],
    fk: ForgeKind,
) {
    let Ok(pr_map) = refresh_pr_map(forge, owner, repo) else { return };
    let Ok(my_bookmarks) = jj.get_my_bookmarks() else { return };
    let orphaned: Vec<_> = my_bookmarks
        .iter()
        .filter(|b| pr_map.contains_key(&b.name))
        .filter(|b| !merged.iter().any(|m| m.bookmark_name == b.name))
        .filter(|b| !skipped.iter().any(|s| s.bookmark_name == b.name))
        .collect();
    if orphaned.is_empty() {
        return;
    }
    let plural = if orphaned.len() == 1 { "" } else { "s" };
    println!("\n  Note: {} open PR{plural} still exist for your bookmarks:", orphaned.len());
    for b in &orphaned {
        if let Some(pr) = pr_map.get(&b.name) {
            println!("    - '{}' ({})", b.name, fk.format_ref(pr.number));
        }
    }
    println!("  These may need manual attention.");
}

/// Print the warnings and recovery hints when reconcile fails inside a
/// watch iteration. Mirrors `print_local_warnings` but tailored for the
/// inline "watch is going to keep trying" context.
fn report_reconcile_failure(
    state: &ReconcileState,
    segments: &[NarrowedSegment],
    merged: &[MergedPr],
    skipped: &[SkippedMergedPr],
    stack_base: Option<&str>,
    default_branch: &str,
    fk: ForgeKind,
) {
    let merged_names: std::collections::HashSet<&str> = merged.iter()
        .map(|m| m.bookmark_name.as_str())
        .chain(skipped.iter().map(|s| s.bookmark_name.as_str()))
        .collect();
    let next_unmerged = segments.iter().find(|s| !merged_names.contains(s.bookmark.name.as_str()));

    let pr_label = next_unmerged
        .map(|s| format!(" '{}'", s.bookmark.name))
        .unwrap_or_default();

    let reasons = state.block_reasons();
    println!();
    println!("  Stopped before merging next PR{pr_label}:");
    for reason in &reasons {
        println!("    - {}", crate::merge::execute::format_block_reason(reason, fk));
    }

    if state.local_failed {
        println!();
        println!("  Local sync warnings:");
        for w in state.warnings.iter().filter(|w| w.kind == DivergenceKind::Local) {
            println!("    {}", w.message);
        }
        if let Some(seg) = next_unmerged {
            println!();
            println!("  To fix locally and continue (watch will resume on the next poll):");
            let base = stack_base.unwrap_or(default_branch);
            // rebase_root: oldest commit in the segment so multi-commit
            // segments don't strand earlier commits.
            println!("    jj git fetch && jj rebase -s {} -d {base}", rebase_root(seg));
            println!("  Or to accept the forge state:");
            println!("    jj git fetch");
            println!("    jj bookmark set {0} -r {0}@origin", seg.bookmark.name);
        }
    }

    if state.forge_failed {
        println!();
        println!("  Forge reconcile warnings:");
        for w in state.warnings.iter().filter(|w| w.kind == DivergenceKind::Forge) {
            println!("    {}", w.message);
        }
        println!();
        println!("  Watch will retry on the next poll. Persistent failures may indicate");
        println!("  a network or forge-permission issue.");
    }
}

/// Re-discover segments by rebuilding the change graph.
///
/// If the target bookmark is no longer in the graph (e.g., it was merged into
/// trunk), falls back to inferring the target from the working copy's position.
/// This handles the case where mid-stack merges change the graph while the
/// leaf bookmark is gone.
fn rediscover_segments(
    jj: &dyn Jj,
    target_bookmark: &str,
) -> Result<Vec<NarrowedSegment>> {
    let graph = change_graph::build_change_graph(jj)?;

    match analyze::analyze_submission_graph(&graph, target_bookmark) {
        Ok(a) => resolve::resolve_bookmark_selections(&a.relevant_segments, false),
        Err(_) => {
            // Target bookmark gone — try inferring from working copy
            if let Ok(Some(inferred)) = analyze::infer_target_bookmark(&graph, jj)
                && let Ok(a) = analyze::analyze_submission_graph(&graph, &inferred)
            {
                return resolve::resolve_bookmark_selections(&a.relevant_segments, false);
            }
            Ok(vec![])
        }
    }
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
    use crate::jj::types::{Bookmark, GitRemote, LogEntry};

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

    // --- wait_for_bookmark stub + tests ---

    /// Jj stub for wait_for_bookmark. `appear_after` controls how many
    /// `get_my_bookmarks` calls return empty before the bookmark appears.
    /// `get_changes_to_commit` always reports the bookmark's change_id in
    /// ancestry, so once the bookmark surfaces, `infer_target_bookmark` returns it.
    struct WaitJj {
        bookmark_name: String,
        bookmark_change_id: String,
        bookmark_commit_id: String,
        wc_commit_id: String,
        appear_after: u32,
        calls: Mutex<u32>,
    }

    impl WaitJj {
        fn new(name: &str, appear_after: u32) -> Self {
            Self {
                bookmark_name: name.to_string(),
                bookmark_change_id: format!("change_{name}"),
                bookmark_commit_id: format!("commit_{name}"),
                wc_commit_id: "wc_commit".to_string(),
                appear_after,
                calls: Mutex::new(0),
            }
        }

        fn log_entry_for_bookmark(&self) -> LogEntry {
            LogEntry {
                commit_id: self.bookmark_commit_id.clone(),
                change_id: self.bookmark_change_id.clone(),
                author_name: "Test".to_string(),
                author_email: "test@test.com".to_string(),
                description: "test".to_string(),
                description_first_line: "test".to_string(),
                parents: vec![],
                local_bookmarks: vec![self.bookmark_name.clone()],
                remote_bookmarks: vec![],
                is_working_copy: false,
                conflict: false,
                empty: false,
            }
        }
    }

    impl Jj for WaitJj {
        fn git_fetch(&self) -> Result<()> { Ok(()) }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            let mut n = self.calls.lock().expect("poisoned");
            *n += 1;
            if *n > self.appear_after {
                Ok(vec![Bookmark {
                    name: self.bookmark_name.clone(),
                    commit_id: self.bookmark_commit_id.clone(),
                    change_id: self.bookmark_change_id.clone(),
                    has_remote: false,
                    is_synced: false,
                }])
            } else {
                Ok(vec![])
            }
        }
        fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> {
            // Ancestry contains the bookmark's change_id, so once
            // `get_my_bookmarks` surfaces it, inference will find an overlap.
            Ok(vec![self.log_entry_for_bookmark()])
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
        fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> { Ok(()) }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok(self.wc_commit_id.clone())
        }
        fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { unimplemented!() }
        fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { unimplemented!() }
        fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
            Ok(vec!["dummy".to_string()])
        }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
    }

    #[test]
    fn wait_for_bookmark_returns_when_bookmark_appears() {
        let jj = WaitJj::new("auth", 1);
        let shutdown = AtomicBool::new(false);

        let result = wait_for_bookmark(
            &jj,
            None,
            Duration::from_millis(1),
            &shutdown,
        ).unwrap();

        assert_eq!(result.as_deref(), Some("auth"));
    }

    #[test]
    fn wait_for_bookmark_respects_shutdown() {
        // Bookmark never appears; pre-set shutdown so the loop exits on the
        // first iteration via the top-of-loop shutdown check.
        let jj = WaitJj::new("auth", u32::MAX);
        let shutdown = AtomicBool::new(true);

        let start = Instant::now();
        let result = wait_for_bookmark(
            &jj,
            None,
            Duration::from_secs(60),
            &shutdown,
        ).unwrap();

        assert!(result.is_none());
        // Should return immediately without waiting on the 60s poll.
        assert!(start.elapsed() < Duration::from_secs(1),
            "expected immediate return, took {:?}", start.elapsed());
    }

    #[test]
    fn wait_for_bookmark_respects_timeout() {
        // Bookmark never appears; deadline expires after one poll iteration.
        let jj = WaitJj::new("auth", u32::MAX);
        let shutdown = AtomicBool::new(false);

        let result = wait_for_bookmark(
            &jj,
            Some(Duration::from_millis(10)),
            Duration::from_millis(1),
            &shutdown,
        ).unwrap();

        assert!(result.is_none());
    }

    // --- run_merge_phase gate tests ---
    //
    // These cover the path where reconcile_after_merge produces warnings
    // mid-iteration. The gate must break the inner loop without merging
    // any subsequent PR, and the ReconcileState must reflect the failure
    // so the outer watch loop can report it and retry on the next poll.

    use crate::merge::execute::{
        DivergenceKind, LocalDivergenceWarning, ReconcileState,
    };
    use crate::merge::plan::{MergeOptions, MergePlan};

    /// Jj stub whose git_fetch fails. Other ops succeed so reconcile_local_state
    /// reaches the fetch call before bailing.
    struct FailFetchJj;
    impl Jj for FailFetchJj {
        fn git_fetch(&self) -> Result<()> {
            anyhow::bail!("ssh: connection refused")
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
        fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".into()) }
        fn push_bookmark(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".into()) }
        fn rebase_onto(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn merge_into(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> { Ok(vec!["c".into()]) }
        fn is_conflicted(&self, _: &str) -> Result<bool> { Ok(false) }
    }

    /// Forge stub that records every merge_pr call and serves the given
    /// PR map for evaluate_segment.
    struct GateForge {
        prs: HashMap<String, PullRequest>,
        merge_calls: std::sync::Mutex<Vec<u64>>,
    }
    impl GateForge {
        fn new(prs: Vec<PullRequest>) -> Self {
            let map = prs.into_iter().map(|p| (p.head.ref_name.clone(), p)).collect();
            Self { prs: map, merge_calls: std::sync::Mutex::new(vec![]) }
        }
        fn merge_calls(&self) -> Vec<u64> {
            self.merge_calls.lock().expect("poisoned").clone()
        }
    }
    impl Forge for GateForge {
        fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
            Ok(self.prs.values().cloned().collect())
        }
        fn merge_pr(&self, _: &str, _: &str, n: u64, _: MergeMethod) -> Result<()> {
            self.merge_calls.lock().expect("poisoned").push(n);
            Ok(())
        }
        fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
            Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".into() })
        }
        fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
            Ok(ChecksStatus::Pass)
        }
        fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary { approved_count: 1, changes_requested: false })
        }
        fn find_merged_pr(&self, _: &str, _: &str, head: &str) -> Result<Option<PullRequest>> {
            // For tests where the bottom is "AlreadyMerged", caller arranges
            // for the bookmark to be in `prs` but with merged state simulated
            // via missing-from-list_open_prs. Simplify: never report merged.
            let _ = head;
            Ok(None)
        }
        fn create_pr(&self, _: &str, _: &str, _: &str, _: &str, _: &str, _: &str, _: bool) -> Result<PullRequest> { unimplemented!() }
        fn update_pr_base(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> { Ok(()) }
        fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> { Ok(()) }
        fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
        fn create_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<IssueComment> { unimplemented!() }
        fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn get_authenticated_user(&self) -> Result<String> { Ok("test".into()) }
        fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
            Ok(PrState { merged: false, state: "open".into() })
        }
    }

    fn gate_test_pr(name: &str, number: u64) -> PullRequest {
        PullRequest {
            number,
            html_url: format!("https://github.com/o/r/pull/{number}"),
            title: name.into(),
            body: None,
            base: PullRequestRef { ref_name: "main".into(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: name.into(), label: String::new(), sha: format!("sha_{name}") },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        }
    }

    fn gate_test_segment(name: &str) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.into(),
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![],
            merge_source_names: vec![],
        }
    }

    fn gate_test_plan() -> MergePlan {
        MergePlan {
            actions: vec![],
            repo_info: RepoInfo { owner: "o".into(), repo: "r".into() },
            forge_kind: ForgeKind::GitHub,
            options: MergeOptions {
                merge_method: MergeMethod::Squash,
                required_approvals: 1,
                require_ci_pass: true,
                reconcile_strategy: crate::config::ReconcileStrategy::Rebase,
                ready: false,
            },
            default_branch: "main".into(),
            remote_name: "origin".into(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        }
    }

    #[test]
    fn run_merge_phase_gates_after_failed_reconcile() {
        // Setup: 2-stack, both mergeable. After auth merges, the next
        // reconcile fails (FailFetchJj). The gate must break the inner
        // loop without calling merge_pr on profile, and ReconcileState
        // must reflect the failure.
        let forge = GateForge::new(vec![
            gate_test_pr("auth", 1),
            gate_test_pr("profile", 2),
        ]);
        let segments = vec![gate_test_segment("auth"), gate_test_segment("profile")];
        let plan = gate_test_plan();
        let mut state = ReconcileState::default();
        let mut prev_reasons: Option<Vec<BlockReason>> = None;
        let mut consecutive_errors = 0u32;
        let mut last_heartbeat = Instant::now();
        let mut dots = false;

        let outcome = run_merge_phase(
            &FailFetchJj, &forge, &segments, &forge.prs, &plan.options,
            &plan, ForgeKind::GitHub,
            &mut prev_reasons, &mut consecutive_errors,
            &mut last_heartbeat, &mut state, &mut dots,
        ).expect("run_merge_phase should not error");

        // auth merged once; profile must NOT have been merged.
        assert_eq!(forge.merge_calls(), vec![1],
            "only auth should merge before the gate fires");
        assert_eq!(outcome.merged.len(), 1);
        assert_eq!(outcome.merged[0].pr_number, 1);

        // Gate uses ReconcileState; outer watch loop reads state.degraded()
        // to decide whether to retry. blocked stays None so the outer loop
        // iterates rather than exits.
        assert!(outcome.blocked.is_none(), "gate should not return Blocked; G semantics");
        assert!(state.degraded(), "reconcile failure must mark state as degraded");
        assert!(state.local_failed, "fetch failure is a local-side failure");
        assert!(!state.forge_failed, "forge side did not fail in this scenario");
        assert!(state.warnings.iter().any(|w| w.kind == DivergenceKind::Local));
    }

    /// Forge whose list_open_prs fails. Inside run_merge_phase, the only
    /// list_open_prs call is from reconcile_forge_state, so this triggers
    /// a forge-side reconcile failure. evaluate_segment uses the pr_map
    /// passed in, not list_open_prs, so the first segment's evaluation
    /// still succeeds.
    struct ListFailForge {
        prs: HashMap<String, PullRequest>,
        merge_calls: std::sync::Mutex<Vec<u64>>,
    }
    impl ListFailForge {
        fn new(prs: Vec<PullRequest>) -> Self {
            let map = prs.into_iter().map(|p| (p.head.ref_name.clone(), p)).collect();
            Self { prs: map, merge_calls: std::sync::Mutex::new(vec![]) }
        }
        fn merge_calls(&self) -> Vec<u64> {
            self.merge_calls.lock().expect("poisoned").clone()
        }
    }
    impl Forge for ListFailForge {
        fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
            anyhow::bail!("502 bad gateway")
        }
        fn merge_pr(&self, _: &str, _: &str, n: u64, _: MergeMethod) -> Result<()> {
            self.merge_calls.lock().expect("poisoned").push(n);
            Ok(())
        }
        fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
            Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".into() })
        }
        fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
            Ok(ChecksStatus::Pass)
        }
        fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary { approved_count: 1, changes_requested: false })
        }
        fn find_merged_pr(&self, _: &str, _: &str, _: &str) -> Result<Option<PullRequest>> { Ok(None) }
        fn create_pr(&self, _: &str, _: &str, _: &str, _: &str, _: &str, _: &str, _: bool) -> Result<PullRequest> { unimplemented!() }
        fn update_pr_base(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> { Ok(()) }
        fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> { Ok(()) }
        fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
        fn create_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<IssueComment> { unimplemented!() }
        fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { Ok(()) }
        fn get_authenticated_user(&self) -> Result<String> { Ok("test".into()) }
        fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
            Ok(PrState { merged: false, state: "open".into() })
        }
    }

    /// Healthy Jj that lets the local-state side of reconcile pass cleanly.
    /// Pairs with ListFailForge to isolate the forge-side failure path.
    struct HealthyJj;
    impl Jj for HealthyJj {
        fn git_fetch(&self) -> Result<()> { Ok(()) }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
        fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".into()) }
        fn push_bookmark(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".into()) }
        fn rebase_onto(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn merge_into(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> { Ok(vec!["c".into()]) }
        fn is_conflicted(&self, _: &str) -> Result<bool> { Ok(false) }
    }

    // --- classify_post_merge tests (the persistent-watch state machine) ---
    //
    // These cover the exact transition logic in run_watch_loop's outer
    // loop. classify_post_merge is pure; the loop dispatches its result
    // to side effects. By testing the classifier exhaustively here, we
    // lock the persistent-retry behavior promised by jjpr watch:
    //
    //   - first time degraded → NewFailure (loud)
    //   - same failure persists → Heartbeat or Dot (quiet)
    //   - state recovers → Recovered (announce)
    //   - clean steady state → Continue (silent)

    fn empty_state() -> ReconcileState {
        ReconcileState::default()
    }

    fn local_failure_state() -> ReconcileState {
        ReconcileState {
            local_failed: true,
            forge_failed: false,
            warnings: vec![LocalDivergenceWarning {
                kind: DivergenceKind::Local,
                message: "fetch failed".into(),
            }],
        }
    }

    fn forge_failure_state() -> ReconcileState {
        ReconcileState {
            local_failed: false,
            forge_failed: true,
            warnings: vec![LocalDivergenceWarning {
                kind: DivergenceKind::Forge,
                message: "list_open_prs failed".into(),
            }],
        }
    }

    const HEARTBEAT: Duration = Duration::from_secs(60);

    #[test]
    fn classify_clean_state_no_prev_is_continue() {
        let action = classify_post_merge(
            &empty_state(), &None, Duration::ZERO, HEARTBEAT,
        );
        assert_eq!(action, PostMergeAction::Continue);
        assert!(!action.waits());
    }

    #[test]
    fn classify_clean_state_after_failure_is_recovered() {
        // Iter N degraded → set prev. Iter N+1 clean → must announce recovery.
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let action = classify_post_merge(&empty_state(), &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(action, PostMergeAction::Recovered);
        assert!(!action.waits());
    }

    #[test]
    fn classify_first_degraded_is_new_failure() {
        let action = classify_post_merge(
            &local_failure_state(), &None, Duration::ZERO, HEARTBEAT,
        );
        assert_eq!(action, PostMergeAction::NewFailure);
        assert!(action.waits());
    }

    #[test]
    fn classify_persistent_same_failure_before_heartbeat_is_dot() {
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let action = classify_post_merge(
            &local_failure_state(), &prev, Duration::from_secs(10), HEARTBEAT,
        );
        assert_eq!(action, PostMergeAction::Dot);
        assert!(action.waits());
    }

    #[test]
    fn classify_persistent_same_failure_after_heartbeat_is_heartbeat() {
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let action = classify_post_merge(
            &local_failure_state(), &prev, Duration::from_secs(120), HEARTBEAT,
        );
        assert_eq!(action, PostMergeAction::Heartbeat);
        assert!(action.waits());
    }

    #[test]
    fn classify_failure_kind_change_is_new_failure() {
        // Iter N had local failure; iter N+1 has a forge failure too.
        // That's a different reason set, so we must reprint full hints.
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let mixed = ReconcileState {
            local_failed: true,
            forge_failed: true,
            warnings: vec![],
        };
        let action = classify_post_merge(&mixed, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(action, PostMergeAction::NewFailure);
    }

    #[test]
    fn classify_local_to_forge_only_is_new_failure() {
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let action = classify_post_merge(
            &forge_failure_state(), &prev, Duration::ZERO, HEARTBEAT,
        );
        assert_eq!(action, PostMergeAction::NewFailure);
    }

    /// B's load-bearing scenario: degrade, persist, recover, succeed.
    /// Drives the classifier through the same sequence the live watch
    /// loop would walk, asserting each transition. If the gate ever
    /// silently stops firing or recovery silently stops printing, this
    /// test catches it deterministically without any forge or jj stubs.
    #[test]
    fn classifier_walks_full_recovery_sequence() {
        let mut prev: Option<Vec<BlockReason>> = None;

        // Iter 1: first degraded poll → NewFailure (full recovery hints).
        let degraded = local_failure_state();
        let a1 = classify_post_merge(&degraded, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(a1, PostMergeAction::NewFailure, "iter 1 must announce");
        prev = Some(degraded.block_reasons());

        // Iter 2: same failure, only 5s elapsed → Dot.
        let a2 = classify_post_merge(&degraded, &prev, Duration::from_secs(5), HEARTBEAT);
        assert_eq!(a2, PostMergeAction::Dot, "iter 2 within heartbeat window must be quiet");
        // prev unchanged

        // Iter 3: same failure, 65s elapsed → Heartbeat.
        let a3 = classify_post_merge(&degraded, &prev, Duration::from_secs(65), HEARTBEAT);
        assert_eq!(a3, PostMergeAction::Heartbeat, "iter 3 past heartbeat must surface");
        // loop resets last_heartbeat when heartbeat fires

        // Iter 4: user fixed it → state clean → Recovered.
        let clean = empty_state();
        let a4 = classify_post_merge(&clean, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(a4, PostMergeAction::Recovered, "iter 4 must announce recovery");
        prev = None;

        // Iter 5: still clean, prev cleared → Continue.
        let a5 = classify_post_merge(&clean, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(a5, PostMergeAction::Continue, "iter 5 returns to silent steady-state");

        // Iter 6: a fresh failure (different kind this time) → NewFailure.
        // Verifies we re-announce instead of staying silent.
        let new_failure = forge_failure_state();
        let a6 = classify_post_merge(&new_failure, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(a6, PostMergeAction::NewFailure, "fresh failure must reannounce");
    }

    #[test]
    fn classifier_does_not_recover_when_already_clean() {
        // prev=None, state clean: Continue (no spurious "recovered" message).
        let a = classify_post_merge(&empty_state(), &None, Duration::ZERO, HEARTBEAT);
        assert_eq!(a, PostMergeAction::Continue);
    }

    #[test]
    fn classifier_treats_zero_heartbeat_correctly() {
        // Edge case: heartbeat_interval = 0. Any persistent failure should
        // print a heartbeat every iteration. Important if we ever expose
        // this via a config flag.
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let a = classify_post_merge(
            &local_failure_state(), &prev, Duration::ZERO, Duration::ZERO,
        );
        assert_eq!(a, PostMergeAction::Heartbeat);
    }

    #[test]
    fn run_merge_phase_gates_after_forge_reconcile_failure() {
        // Mirrors the local-failure test but on the forge side: local
        // reconcile passes cleanly, then forge-state reconcile fails on
        // list_open_prs. The gate must still fire and the state must
        // tag warnings as Forge-kind so users get the right recovery hint.
        let forge = ListFailForge::new(vec![
            gate_test_pr("auth", 1),
            gate_test_pr("profile", 2),
        ]);
        let segments = vec![gate_test_segment("auth"), gate_test_segment("profile")];
        let plan = gate_test_plan();
        let mut state = ReconcileState::default();
        let mut prev_reasons: Option<Vec<BlockReason>> = None;
        let mut consecutive_errors = 0u32;
        let mut last_heartbeat = Instant::now();
        let mut dots = false;

        let outcome = run_merge_phase(
            &HealthyJj, &forge, &segments, &forge.prs, &plan.options,
            &plan, ForgeKind::GitHub,
            &mut prev_reasons, &mut consecutive_errors,
            &mut last_heartbeat, &mut state, &mut dots,
        ).expect("run_merge_phase should not error");

        assert_eq!(forge.merge_calls(), vec![1], "only auth should merge");
        assert!(outcome.blocked.is_none(), "gate keeps watch iterating");
        assert!(state.degraded());
        assert!(!state.local_failed, "local side was healthy");
        assert!(state.forge_failed, "list_open_prs failure must set forge_failed");
        assert!(
            state.warnings.iter().any(|w| w.kind == DivergenceKind::Forge),
            "must record a Forge-kind warning"
        );
    }
}
