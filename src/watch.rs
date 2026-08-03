use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::forge::types::{ChecksStatus, PullRequest, RepoInfo};
use crate::forge::{Forge, ForgeKind};
use crate::graph::change_graph;
use crate::jj::Jj;
use crate::jj::types::NarrowedSegment;
use crate::merge::execute::{
    BlockedPr, DivergenceKind, MergeResult, MergedPr, ReconcileState, SkippedMergedPr,
    format_block_reason, merge_with_retry, rebase_root, reconcile_after_merge,
};
use crate::merge::plan::{BlockReason, MergeOptions, PrMergeStatus, evaluate_segment};
use crate::merge::watch::{
    HEARTBEAT_INTERVAL, MAX_CONSECUTIVE_ERRORS, WatchOptions, clear_status_line, local_time_hhmm,
    refresh_pr_map, report_status_changes, should_print_heartbeat, spinner_sleep,
};
use crate::submit::{analyze, execute, plan, resolve};

/// Submit-phase options for the watch loop. Mirrors the relevant
/// surface of `submit::plan::SubmitOptions` so watch and submit can't
/// drift on user-visible knobs. Owned (no lifetimes) so the watch loop
/// can hold it across the long-running poll.
///
/// `draft_mode` is the same enum submit uses. Watch's CLI exposes a
/// boolean `--ready` flag and translates it to the appropriate
/// `DraftMode` variant at command-dispatch time.
#[derive(Debug, Clone)]
pub struct WatchSubmitOptions {
    /// Reviewers to request on each iteration's newly-created or
    /// existing PRs that match `reviewer_scope`. Empty = no requests.
    pub reviewers: Vec<String>,
    /// Which segments receive reviewer requests this iteration.
    pub reviewer_scope: crate::forge::types::ReviewerScope,
    /// Lifecycle mode for new PRs and existing drafts. Watch's natural
    /// state is `NewAsDraft` (create as draft, promote when CI passes).
    /// `MarkExistingReady` is `--ready`'s mapping (treat the stack as
    /// ready: mark existing drafts and create new as ready). `Default`
    /// is rarely useful from watch but accepted for consistency.
    pub draft_mode: crate::submit::plan::DraftMode,
}

impl Default for WatchSubmitOptions {
    fn default() -> Self {
        Self {
            reviewers: Vec::new(),
            reviewer_scope: crate::forge::types::ReviewerScope::Bottom,
            draft_mode: crate::submit::plan::DraftMode::NewAsDraft,
        }
    }
}

impl WatchSubmitOptions {
    /// Construct from the watch CLI's flag surface. The `ready` flag
    /// is a single bool exposed at the CLI; this conversion mirrors
    /// `submit --ready`'s semantics so the two commands behave the
    /// same way for the same flag.
    pub fn from_cli(
        reviewers: Vec<String>,
        reviewer_scope: crate::forge::types::ReviewerScope,
        ready: bool,
    ) -> Self {
        Self {
            reviewers,
            reviewer_scope,
            draft_mode: if ready {
                crate::submit::plan::DraftMode::MarkExistingReady
            } else {
                crate::submit::plan::DraftMode::NewAsDraft
            },
        }
    }
}

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

    // Read every draft's CI in one go. This runs on every poll, so the old
    // request-per-draft added up; batching keeps a wide stack's poll the same
    // cost as a narrow one. Only CI is asked for: nothing below reads the rest
    // of a status bundle, and on GitLab a review lookup alone is three requests.
    let drafts: Vec<&PullRequest> = segments
        .iter()
        .filter_map(|seg| pr_map.get(&seg.bookmark.name))
        .filter(|pr| pr.draft)
        .collect();
    let checks = crate::forge::status::fetch_checks(forge, repo_info, &drafts);

    for seg in segments {
        let Some(pr) = pr_map.get(&seg.bookmark.name) else {
            continue;
        };
        if !pr.draft {
            continue;
        }

        // Absent means the CI read failed; leave the draft alone rather than
        // promote it on an unknown.
        let Some(status) = checks.get(&pr.number) else {
            continue;
        };

        if *status == ChecksStatus::Pass {
            if let Err(e) = forge.mark_pr_ready(owner, repo, pr.number) {
                eprintln!(
                    "  Warning: failed to mark {} as ready: {e}",
                    fk.format_ref(pr.number)
                );
                continue;
            }
            println!("  Marked '{}' as ready (CI passing)", seg.bookmark.name);
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
    if !reasons
        .iter()
        .any(|r| matches!(r, BlockReason::InsufficientApprovals { .. }))
    {
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
    /// The merge phase stopped early because a segment is still blocked and
    /// we're waiting for it to clear (CI pending, awaiting approval, still a
    /// draft) — as opposed to a hard block (`blocked`) or running to
    /// completion. This is an *active wait*, not a stall: the outer loop must
    /// not count it toward the no-progress safety valve, or `jjpr watch`
    /// abandons slow CI after five polls (issue #4).
    waiting_on_block: bool,
}

/// Whether a just-finished merge phase represents a genuine *stall* that
/// should count toward the no-progress safety valve.
///
/// A stall is: the phase ran without stopping to wait on a block, yet nothing
/// merged/skipped changed and nothing was created or promoted. That happens
/// when `rediscover_segments` keeps handing back the same already-merged
/// segments — a real loop we must break out of.
///
/// An active wait on a still-blocked segment is not a stall: watch must keep
/// polling until the user's `--timeout` fires (issue #4).
fn is_stalled(
    waiting_on_block: bool,
    total_before: usize,
    total_after: usize,
    created_or_promoted: bool,
) -> bool {
    !waiting_on_block && total_after == total_before && !created_or_promoted
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
    /// heartbeat interval has elapsed. Sleep and retry quietly; the between-poll
    /// spinner (TTY) or the next heartbeat (non-TTY) carries the update.
    Quiet,
}

impl PostMergeAction {
    /// Whether the loop should sleep and `continue` after handling this
    /// action. True for any degraded action; false for clean ones.
    fn waits(self) -> bool {
        matches!(self, Self::NewFailure | Self::Heartbeat | Self::Quiet)
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
        PostMergeAction::Quiet
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
    is_tty: bool,
) -> Result<MergePhaseOutcome> {
    let owner = &merge_plan.repo_info.owner;
    let repo = &merge_plan.repo_info.repo;
    let mut pr_map = pr_map.clone();
    let mut merged = Vec::new();
    let mut skipped = Vec::new();
    let mut seg_idx = 0;
    let mut advanced = false;
    let mut waiting_on_block = false;

    while seg_idx < segments.len() {
        let segment = &segments[seg_idx];
        let status = match evaluate_segment(
            forge,
            &segment.bookmark.name,
            &merge_plan.repo_info,
            &pr_map,
            merge_options,
            // Not prefetched, for the same reason execute doesn't: this loop
            // merges as it goes, and each merge moves the next segment's base.
            // A batch taken before it started would be stale by the time the
            // later segments were read.
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                *consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!(
                    "  [{now}] Eval error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}"
                );
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
                    // Match execute_merge_plan's UX: name the bookmark so
                    // the user knows where the stack stopped.
                    println!("\n  Blocked at '{bookmark_name}':");
                    println!(
                        "    - {}",
                        format_block_reason(&BlockReason::NoPr, forge_kind)
                    );
                    return Ok(MergePhaseOutcome {
                        merged,
                        skipped,
                        blocked: Some(BlockedPr {
                            bookmark_name,
                            pr_number: None,
                            reasons,
                        }),
                        all_done: false,
                        waiting_on_block: false,
                    });
                }

                // Native-stack membership never clears on its own, so polling
                // would spin at the poll interval forever. Stop and name the
                // thing that will actually resolve it, as with NoPr above.
                if let Some(line) = reasons
                    .iter()
                    .find(|r| matches!(r, BlockReason::NativeStack { .. }))
                    .map(|r| format_block_reason(r, forge_kind))
                {
                    println!("\n  Blocked at '{bookmark_name}':");
                    println!("    - {line}");
                    let pr_number = pr.as_ref().map(|p| p.number);
                    return Ok(MergePhaseOutcome {
                        merged,
                        skipped,
                        blocked: Some(BlockedPr {
                            bookmark_name,
                            pr_number,
                            reasons,
                        }),
                        all_done: false,
                        waiting_on_block: false,
                    });
                }

                if prev_reasons.is_none()
                    && let Some(hint) =
                        reviewer_hint(pr.as_ref(), &reasons, &bookmark_name, forge_kind)
                {
                    println!("{hint}");
                }

                match report_status_changes(
                    &bookmark_name,
                    prev_reasons.as_deref(),
                    &reasons,
                    forge_kind,
                ) {
                    Some(displayed) => {
                        *prev_reasons = Some(displayed);
                        *last_heartbeat = Instant::now();
                    }
                    None => {
                        if prev_reasons.is_none() {
                            *prev_reasons = Some(vec![]);
                        }
                        // On a TTY the between-poll spinner is the liveness
                        // signal; off a TTY, print a periodic timestamped line
                        // so a long wait doesn't look hung.
                        if should_print_heartbeat(
                            is_tty,
                            last_heartbeat.elapsed(),
                            HEARTBEAT_INTERVAL,
                        ) {
                            let now = local_time_hhmm();
                            let first_reason = reasons
                                .first()
                                .map(|r| format_block_reason(r, forge_kind))
                                .unwrap_or_default();
                            println!("  [{now}] Still waiting for {bookmark_name}: {first_reason}");
                            *last_heartbeat = Instant::now();
                        }
                    }
                }
                waiting_on_block = true;
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
                Some(&pr_map),
                state,
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
                break;
            }
        }
    }

    Ok(MergePhaseOutcome {
        merged,
        skipped,
        blocked: None,
        all_done: seg_idx >= segments.len() && advanced,
        waiting_on_block,
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
    is_tty: bool,
    heartbeat: Option<&crate::heartbeat::WatchHeartbeat>,
) -> Result<Option<String>> {
    let deadline = timeout.map(|d| Instant::now() + d);

    println!("Waiting for a bookmark in the working copy's ancestry...");
    println!("    hint: jj bookmark set <name>\n");

    let mut spinner_frame: usize = 0;
    loop {
        // Keep the watcher's heartbeat fresh while it waits, so a second
        // `jjpr watch` started during a long bookmark-wait still backs off.
        if let Some(hb) = heartbeat {
            hb.refresh();
        }
        if shutdown.load(Ordering::Relaxed) {
            clear_status_line(&mut std::io::stdout(), is_tty);
            return Ok(None);
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            clear_status_line(&mut std::io::stdout(), is_tty);
            return Ok(None);
        }

        // The bookmark check is silent, so the spinner frame left by
        // spinner_sleep stays on screen through it — no blank flash before the
        // spin resumes. Clear only before returning to a caller that prints
        // (found / timeout).
        if let Ok(graph) = change_graph::build_change_graph(jj)
            && let Ok(Some(name)) = analyze::infer_target_bookmark(&graph, jj)
        {
            clear_status_line(&mut std::io::stdout(), is_tty);
            return Ok(Some(name));
        }

        if spinner_sleep(poll_interval, shutdown, is_tty, &mut spinner_frame) {
            return Ok(None);
        }
    }
}

// `too_many_lines` fires at 284/275 only because rustfmt's 2024 style edition
// splits argument lists one-per-line; the body did not gain a statement or a
// branch. Allowed rather than silenced globally via clippy.toml, so the limit
// keeps applying everywhere else and this stays the one visible exception.
//
// It is a real signal even so — the function is long enough to be worth
// decomposing, which is deferred work rather than something to do inside a
// pure-formatting commit (see TODO.md).
#[allow(
    clippy::too_many_arguments,
    clippy::cognitive_complexity,
    clippy::too_many_lines
)]
pub fn run_watch_loop(
    jj: &dyn Jj,
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    forge_kind: ForgeKind,
    remote_name: &str,
    default_branch: &str,
    merge_options: &MergeOptions,
    submit_opts: &WatchSubmitOptions,
    target_bookmark: &str,
    stack_base: Option<&str>,
    stack_nav: crate::config::StackNavMode,
    opts: WatchOptions,
    heartbeat: Option<&crate::heartbeat::WatchHeartbeat>,
) -> Result<WatchResult> {
    let shutdown = opts.shutdown;
    let timeout = opts.timeout;
    let poll_interval = opts.poll_interval;
    let is_tty = opts.is_tty;
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
    let mut no_progress_count: u32 = 0;
    let mut spinner_frame: usize = 0;
    let deadline = timeout.map(|d| Instant::now() + d);

    let merge_plan = make_merge_plan(
        repo_info,
        forge_kind,
        default_branch,
        remote_name,
        merge_options,
        stack_base,
        stack_nav,
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
        // Mark this watcher alive so a second `jjpr watch` here backs off.
        if let Some(hb) = heartbeat {
            hb.refresh();
        }
        // Clear any spinner frame left by the previous poll's sleep before this
        // iteration prints anything, so real messages never collide with it.
        clear_status_line(&mut std::io::stdout(), is_tty);
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
                eprintln!(
                    "  [{now}] Graph scan error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}"
                );
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    eprintln!("  Too many consecutive errors; giving up.");
                    break;
                }
                if spinner_sleep(poll_interval, &shutdown, is_tty, &mut spinner_frame) {
                    break;
                }
                continue;
            }
        };

        if segments.is_empty() {
            println!(
                "\n  Watched stack '{target_bookmark}' is no longer present — it has \
                 merged, or the bookmark was removed. Stopping."
            );
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
        let has_conflicts = segments
            .iter()
            .any(|seg| seg.changes.iter().any(|c| c.conflict));
        if has_conflicts {
            if prev_reasons.is_none() {
                let conflicted: Vec<_> = segments
                    .iter()
                    .flat_map(|seg| {
                        seg.changes
                            .iter()
                            .filter(|c| c.conflict)
                            .map(|c| (seg.bookmark.name.as_str(), c.change_id.as_str()))
                    })
                    .collect();
                println!("\n  Waiting for conflict resolution:");
                for (bookmark, change_id) in &conflicted {
                    println!("    - {change_id} ({bookmark})");
                }
                println!(
                    "    hint: jj edit <change_id>, fix the conflicts, then jjpr watch will continue"
                );
            }
            if spinner_sleep(poll_interval, &shutdown, is_tty, &mut spinner_frame) {
                break;
            }
            continue;
        }

        // --- Phase 2: Submit (push + create draft PRs) ---
        let bookmarks_being_created = match run_submit_phase(
            jj,
            forge,
            &segments,
            remote_name,
            repo_info,
            forge_kind,
            default_branch,
            stack_base,
            stack_nav,
            submit_opts,
        ) {
            Ok(names) => {
                consecutive_errors = 0;
                names
            }
            Err(e) => {
                consecutive_errors += 1;
                let now = local_time_hhmm();
                eprintln!(
                    "  [{now}] Submit error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}"
                );
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    break;
                }
                if spinner_sleep(poll_interval, &shutdown, is_tty, &mut spinner_frame) {
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
                eprintln!(
                    "  [{now}] PR refresh error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}"
                );
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    break;
                }
                if spinner_sleep(poll_interval, &shutdown, is_tty, &mut spinner_frame) {
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
            jj,
            forge,
            &segments,
            &pr_map,
            merge_options,
            &merge_plan,
            forge_kind,
            &mut prev_reasons,
            &mut consecutive_errors,
            &mut last_heartbeat,
            &mut state,
            is_tty,
        )?;

        let total_before = merged.len() + skipped_merged.len();
        merged.extend(merge_outcome.merged);
        // Dedup skipped to avoid re-counting the same AlreadyMerged bookmark
        for s in merge_outcome.skipped {
            if !skipped_merged
                .iter()
                .any(|existing| existing.bookmark_name == s.bookmark_name)
            {
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
            &state,
            &prev_reconcile_block,
            last_heartbeat.elapsed(),
            HEARTBEAT_INTERVAL,
        );
        match action {
            PostMergeAction::Continue => {}
            PostMergeAction::Recovered => {
                println!("  Local sync recovered. Resuming.");
                prev_reconcile_block = None;
            }
            PostMergeAction::NewFailure => {
                report_reconcile_failure(
                    &state,
                    &segments,
                    &merged,
                    &skipped_merged,
                    stack_base,
                    default_branch,
                    forge_kind,
                );
                prev_reconcile_block = Some(state.block_reasons());
                last_heartbeat = Instant::now();
            }
            PostMergeAction::Heartbeat => {
                // On a TTY the between-poll spinner conveys liveness, so the
                // periodic heartbeat is suppressed here too (the one-time
                // NewFailure report already explained what to fix).
                if !is_tty {
                    let now = local_time_hhmm();
                    println!("  [{now}] Still waiting for local sync to recover");
                }
                last_heartbeat = Instant::now();
            }
            PostMergeAction::Quiet => {}
        }
        if action.waits() {
            if spinner_sleep(poll_interval, &shutdown, is_tty, &mut spinner_frame) {
                break;
            }
            continue;
        }

        // No-progress safety valve: must run even when all_done fires, because
        // rediscover_segments might keep returning the same already-merged
        // segments. But an active wait on a still-blocked segment (CI pending,
        // awaiting approval) is not a stall — only --timeout should end that
        // (issue #4). See is_stalled.
        if is_stalled(
            merge_outcome.waiting_on_block,
            total_before,
            total_after,
            created_or_promoted,
        ) {
            no_progress_count += 1;
            if no_progress_count >= 5 {
                println!(
                    "\n  No progress after {no_progress_count} consecutive iterations; exiting."
                );
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

        // Sleep before next iteration, showing the between-poll spinner.
        if spinner_sleep(poll_interval, &shutdown, is_tty, &mut spinner_frame) {
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
    let with_pr: Vec<_> = segments
        .iter()
        .filter(|s| pr_map.contains_key(&s.bookmark.name))
        .collect();
    let without_pr: Vec<_> = segments
        .iter()
        .filter(|s| !pr_map.contains_key(&s.bookmark.name))
        .collect();
    if with_pr.is_empty() && without_pr.is_empty() {
        return;
    }
    let plural = if segments.len() == 1 { "" } else { "s" };
    let with_pr_suffix = if !with_pr.is_empty() {
        format!(
            ", {} with existing PR{}",
            with_pr.len(),
            if with_pr.len() == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    println!(
        "  {} bookmark{plural} in stack{with_pr_suffix}",
        segments.len()
    );
    if !without_pr.is_empty() {
        let names: Vec<_> = without_pr
            .iter()
            .map(|s| s.bookmark.name.as_str())
            .collect();
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
    let Ok(pr_map) = refresh_pr_map(forge, owner, repo) else {
        return;
    };
    let Ok(my_bookmarks) = jj.get_my_bookmarks() else {
        return;
    };
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
    println!(
        "\n  Note: {} open PR{plural} still exist for your bookmarks:",
        orphaned.len()
    );
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
    let merged_names: std::collections::HashSet<&str> = merged
        .iter()
        .map(|m| m.bookmark_name.as_str())
        .chain(skipped.iter().map(|s| s.bookmark_name.as_str()))
        .collect();
    let next_unmerged = segments
        .iter()
        .find(|s| !merged_names.contains(s.bookmark.name.as_str()));

    let pr_label = next_unmerged
        .map(|s| format!(" '{}'", s.bookmark.name))
        .unwrap_or_default();

    let reasons = state.block_reasons();
    println!();
    println!("  Stopped before merging next PR{pr_label}:");
    for reason in &reasons {
        println!(
            "    - {}",
            crate::merge::execute::format_block_reason(reason, fk)
        );
    }

    if state.has_concurrent() {
        println!();
        println!("  Concurrent modification:");
        for w in state
            .warnings
            .iter()
            .filter(|w| w.kind == DivergenceKind::Concurrent)
        {
            println!("    {}", w.message);
        }
        // No manual-fix hint: the warning already states that both sides' work
        // is preserved and watch retries next poll. Recovery never discards work,
        // so there's nothing for the user to restore.
    }

    if state.local_failed {
        println!();
        println!("  Local sync warnings:");
        for w in state
            .warnings
            .iter()
            .filter(|w| w.kind == DivergenceKind::Local)
        {
            println!("    {}", w.message);
        }
        if let Some(seg) = next_unmerged {
            println!();
            println!("  To fix locally and continue (watch will resume on the next poll):");
            let base = stack_base.unwrap_or(default_branch);
            // rebase_root: oldest commit in the segment so multi-commit
            // segments don't strand earlier commits.
            println!(
                "    jj git fetch && jj rebase -s {} -d {base}",
                rebase_root(seg)
            );
            println!("  Or to accept the forge state:");
            println!("    jj git fetch");
            println!("    jj bookmark set {0} -r {0}@origin", seg.bookmark.name);
        }
    }

    if state.forge_failed {
        println!();
        println!("  Forge reconcile warnings:");
        for w in state
            .warnings
            .iter()
            .filter(|w| w.kind == DivergenceKind::Forge)
        {
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
fn rediscover_segments(jj: &dyn Jj, target_bookmark: &str) -> Result<Vec<NarrowedSegment>> {
    let graph = change_graph::build_change_graph(jj)?;

    // The target was chosen intentionally — named explicitly, or inferred from
    // the working copy at startup. If it can no longer be resolved it is
    // genuinely GONE (the stack merged, or the bookmark was removed). We do NOT
    // fall back to inferring from the working copy here: that would silently
    // pivot watch onto whatever stack `@` happens to be on and auto-merge PRs
    // the user never asked to watch. Every resilience case — a rebase moving the
    // bookmark, a bottom squash-merge, a top conflict — keeps the target
    // findable (proven in the rediscover_* tests), so no fallback is needed.
    // Empty means "stop": the loop reports and exits.
    match analyze::analyze_submission_graph(&graph, target_bookmark) {
        Ok(a) => resolve::resolve_bookmark_selections(&a.relevant_segments, false),
        Err(_) => Ok(vec![]),
    }
}

/// Run the submit phase: push unsynced bookmarks, create draft PRs, update bases/bodies.
///
/// Returns the names of bookmarks that had new PRs created. The caller resolves
/// PR numbers from the PR map (which is refreshed immediately after this phase),
/// avoiding an extra list_open_prs API call.
#[allow(clippy::too_many_arguments)]
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
    submit_opts: &WatchSubmitOptions,
) -> Result<Vec<String>> {
    let submission_plan = plan::create_submission_plan(
        forge,
        segments,
        remote_name,
        repo_info,
        forge_kind,
        default_branch,
        &plan::SubmitOptions {
            draft_mode: submit_opts.draft_mode,
            reviewers: &submit_opts.reviewers,
            reviewer_scope: submit_opts.reviewer_scope,
            stack_base,
            stack_nav,
            // dry_run is meaningless inside an infinite watch loop;
            // `cmd_watch` rejects --dry-run at command entry.
            dry_run: false,
        },
    )?;

    // A native-stack base conflict is not an "action" (nothing would be
    // modified), so has_actions() is false for a plan carrying only that. Fall
    // through anyway: execute_submission_plan turns it into an explanatory
    // error, and returning early here would leave watch looping forever with no
    // indication of why submit never does anything.
    if !submission_plan.has_actions() && submission_plan.native_stack_base_conflicts.is_empty() {
        return Ok(vec![]);
    }

    let creating: Vec<String> = submission_plan
        .bookmarks_needing_pr
        .iter()
        .map(|b| b.bookmark.name.clone())
        .collect();

    execute::execute_submission_plan(jj, forge, &submission_plan)?;

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
            author: String::new(),
            stack: None,
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

        /// Register a PR with no checks entry, so the lookup errors.
        fn with_unreadable_checks(mut self, pr: PullRequest) -> Self {
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
            self.checks
                .get(ref_name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no checks for {ref_name}"))
        }
        fn mark_pr_ready(&self, _o: &str, _r: &str, number: u64) -> Result<()> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("mark_pr_ready:{number}"));
            Ok(())
        }
        fn create_pr(
            &self,
            _o: &str,
            _r: &str,
            _t: &str,
            _b: &str,
            _h: &str,
            _ba: &str,
            _d: bool,
        ) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            Ok(())
        }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            Ok(())
        }
        fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> {
            Ok(())
        }
        fn list_comments(&self, _o: &str, _r: &str, _n: u64) -> Result<Vec<IssueComment>> {
            Ok(vec![])
        }
        fn create_comment(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> {
            Ok(())
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("user".to_string())
        }
        fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> {
            Ok(None)
        }
        fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> {
            Ok(())
        }
        fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary {
                approved_count: 0,
                changes_requested: false,
            })
        }
        fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> {
            Ok(PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".to_string(),
            })
        }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
            Ok(PrState {
                merged: false,
                state: "open".to_string(),
            })
        }
    }

    // --- Reviewer hint tests ---

    /// A: watch's --ready maps to MarkExistingReady so existing drafts
    /// get marked ready alongside new PRs being created as ready. Without
    /// this, --ready leaves existing drafts as drafts (relying on the
    /// promote phase + CI), which surprises users coming from `submit
    /// --ready` semantics.
    #[test]
    fn watch_submit_options_from_cli_with_ready_marks_existing() {
        let opts = WatchSubmitOptions::from_cli(
            vec!["alice".into()],
            crate::forge::types::ReviewerScope::Bottom,
            true, // --ready
        );
        assert_eq!(
            opts.draft_mode,
            crate::submit::plan::DraftMode::MarkExistingReady
        );
    }

    #[test]
    fn watch_submit_options_from_cli_without_ready_uses_new_as_draft() {
        let opts = WatchSubmitOptions::from_cli(
            Vec::new(),
            crate::forge::types::ReviewerScope::Bottom,
            false,
        );
        assert_eq!(opts.draft_mode, crate::submit::plan::DraftMode::NewAsDraft);
    }

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

        assert!(
            hint.is_none(),
            "should not show hint when reviewers are present"
        );
    }

    #[test]
    fn test_reviewer_hint_not_shown_for_non_approval_blocks() {
        let pr = make_pr("auth", 42, false);
        let reasons = vec![BlockReason::ChecksPending];

        let hint = reviewer_hint(Some(&pr), &reasons, "auth", ForgeKind::GitHub);

        assert!(
            hint.is_none(),
            "should not show hint for non-approval blocks"
        );
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
        let forge = PromotionForge::new().with_pr(make_pr("auth", 1, true), ChecksStatus::Pass);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].pr_number, 1);
        assert!(forge.calls().contains(&"mark_pr_ready:1".to_string()));
    }

    #[test]
    fn test_no_promote_when_ci_pending() {
        let forge = PromotionForge::new().with_pr(make_pr("auth", 1, true), ChecksStatus::Pending);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty());
        assert!(!forge.calls().iter().any(|c| c.starts_with("mark_pr_ready")));
    }

    #[test]
    fn test_no_promote_when_ci_failing() {
        let forge = PromotionForge::new().with_pr(make_pr("auth", 1, true), ChecksStatus::Fail);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty());
    }

    #[test]
    fn test_no_promote_when_ci_cannot_be_read() {
        // An unreadable CI result must leave the draft alone. Promoting on an
        // unknown would mark a PR ready off the back of a failed request.
        let forge = PromotionForge::new().with_unreadable_checks(make_pr("auth", 1, true));
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty());
        assert!(!forge.calls().iter().any(|c| c.starts_with("mark_pr_ready")));
    }

    #[test]
    fn test_promotion_reads_ci_once_per_poll_not_per_draft() {
        // This runs every 30s, so a request per draft added up. Only CI is read:
        // nothing here looks at reviews, and on GitLab that alone is 3 requests.
        let forge = PromotionForge::new()
            .with_pr(make_pr("auth", 1, true), ChecksStatus::Pass)
            .with_pr(make_pr("api", 2, true), ChecksStatus::Pass)
            .with_pr(make_pr("ui", 3, true), ChecksStatus::Pass);
        let segments = vec![
            make_segment("auth"),
            make_segment("api"),
            make_segment("ui"),
        ];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert_eq!(promoted.len(), 3);
        assert!(
            !forge
                .calls()
                .iter()
                .any(|c| c.starts_with("get_pr_reviews")),
            "promotion must never pay for reviews it does not read",
        );
    }

    #[test]
    fn test_no_promote_when_not_draft() {
        let forge = PromotionForge::new().with_pr(make_pr("auth", 1, false), ChecksStatus::Pass);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(promoted.is_empty());
    }

    #[test]
    fn test_no_promote_when_no_ci_checks() {
        let forge = PromotionForge::new().with_pr(make_pr("auth", 1, true), ChecksStatus::None);
        let segments = vec![make_segment("auth")];
        let pr_map: HashMap<String, PullRequest> = forge.prs.clone();

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert!(
            promoted.is_empty(),
            "should not promote when no CI checks exist"
        );
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

        let promoted =
            promote_ready_drafts(&forge, &segments, &pr_map, &repo_info(), ForgeKind::GitHub);

        assert_eq!(promoted.len(), 3);
        let calls = forge.calls();
        assert!(calls.contains(&"mark_pr_ready:1".to_string()));
        assert!(calls.contains(&"mark_pr_ready:2".to_string()));
        assert!(calls.contains(&"mark_pr_ready:3".to_string()));
    }

    // --- is_stalled (no-progress safety valve) ---

    #[test]
    fn stalled_when_nothing_changed_and_not_waiting() {
        // Merge phase ran to completion, nothing merged, nothing created:
        // rediscover keeps handing back already-merged segments. A real stall.
        assert!(is_stalled(false, 2, 2, false));
    }

    #[test]
    fn not_stalled_while_waiting_on_block() {
        // Regression test for issue #4: a segment blocked on pending CI or an
        // awaited approval is an active wait, not a stall — the no-progress
        // valve must not fire, so --timeout alone governs how long we wait.
        assert!(!is_stalled(true, 2, 2, false));
    }

    #[test]
    fn not_stalled_when_a_merge_landed() {
        assert!(!is_stalled(false, 1, 2, false));
    }

    #[test]
    fn not_stalled_when_something_was_created_or_promoted() {
        assert!(!is_stalled(false, 2, 2, true));
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
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
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
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
            Ok(vec![])
        }
        fn get_default_branch(&self) -> Result<String> {
            Ok("main".to_string())
        }
        fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> {
            Ok(())
        }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok(self.wc_commit_id.clone())
        }
        fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> {
            unimplemented!()
        }
        fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> {
            unimplemented!()
        }
        fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
            Ok(vec!["dummy".to_string()])
        }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn wait_for_bookmark_returns_when_bookmark_appears() {
        let jj = WaitJj::new("auth", 1);
        let shutdown = AtomicBool::new(false);

        let result =
            wait_for_bookmark(&jj, None, Duration::from_millis(1), &shutdown, false, None).unwrap();

        assert_eq!(result.as_deref(), Some("auth"));
    }

    #[test]
    fn wait_for_bookmark_respects_shutdown() {
        // Bookmark never appears; pre-set shutdown so the loop exits on the
        // first iteration via the top-of-loop shutdown check.
        let jj = WaitJj::new("auth", u32::MAX);
        let shutdown = AtomicBool::new(true);

        let start = Instant::now();
        let result =
            wait_for_bookmark(&jj, None, Duration::from_secs(60), &shutdown, false, None).unwrap();

        assert!(result.is_none());
        // Should return immediately without waiting on the 60s poll.
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "expected immediate return, took {:?}",
            start.elapsed()
        );
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
            false,
            None,
        )
        .unwrap();

        assert!(result.is_none());
    }

    // --- run_merge_phase gate tests ---
    //
    // These cover the path where reconcile_after_merge produces warnings
    // mid-iteration. The gate must break the inner loop without merging
    // any subsequent PR, and the ReconcileState must reflect the failure
    // so the outer watch loop can report it and retry on the next poll.

    use crate::merge::execute::{DivergenceKind, LocalDivergenceWarning, ReconcileState};
    use crate::merge::plan::{MergeOptions, MergePlan};

    /// Jj stub whose git_fetch fails. Other ops succeed so reconcile_local_state
    /// reaches the fetch call before bailing.
    struct FailFetchJj;
    impl Jj for FailFetchJj {
        fn git_fetch(&self) -> Result<()> {
            anyhow::bail!("ssh: connection refused")
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(vec![])
        }
        fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> {
            Ok(vec![])
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
            Ok(vec![])
        }
        fn get_default_branch(&self) -> Result<String> {
            Ok("main".into())
        }
        fn push_bookmark(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok("wc".into())
        }
        fn rebase_onto(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn merge_into(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> {
            Ok(vec!["c".into()])
        }
        fn is_conflicted(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
    }

    /// Forge stub that records every merge_pr call and serves the given
    /// PR map for evaluate_segment.
    struct GateForge {
        prs: HashMap<String, PullRequest>,
        merge_calls: std::sync::Mutex<Vec<u64>>,
    }
    impl GateForge {
        fn new(prs: Vec<PullRequest>) -> Self {
            let map = prs
                .into_iter()
                .map(|p| (p.head.ref_name.clone(), p))
                .collect();
            Self {
                prs: map,
                merge_calls: std::sync::Mutex::new(vec![]),
            }
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
            Ok(PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".into(),
            })
        }
        fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
            Ok(ChecksStatus::Pass)
        }
        fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary {
                approved_count: 1,
                changes_requested: false,
            })
        }
        fn find_merged_pr(&self, _: &str, _: &str, head: &str) -> Result<Option<PullRequest>> {
            // For tests where the bottom is "AlreadyMerged", caller arranges
            // for the bookmark to be in `prs` but with merged state simulated
            // via missing-from-list_open_prs. Simplify: never report merged.
            let _ = head;
            Ok(None)
        }
        fn create_pr(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> {
            Ok(())
        }
        fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> {
            Ok(())
        }
        fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> {
            Ok(vec![])
        }
        fn create_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("test".into())
        }
        fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
            Ok(PrState {
                merged: false,
                state: "open".into(),
            })
        }
    }

    fn gate_test_pr(name: &str, number: u64) -> PullRequest {
        PullRequest {
            number,
            html_url: format!("https://github.com/o/r/pull/{number}"),
            title: name.into(),
            body: None,
            base: PullRequestRef {
                ref_name: "main".into(),
                label: String::new(),
                sha: String::new(),
            },
            head: PullRequestRef {
                ref_name: name.into(),
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
            repo_info: RepoInfo {
                owner: "o".into(),
                repo: "r".into(),
            },
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
        let forge = GateForge::new(vec![gate_test_pr("auth", 1), gate_test_pr("profile", 2)]);
        let segments = vec![gate_test_segment("auth"), gate_test_segment("profile")];
        let plan = gate_test_plan();
        let mut state = ReconcileState::default();
        let mut prev_reasons: Option<Vec<BlockReason>> = None;
        let mut consecutive_errors = 0u32;
        let mut last_heartbeat = Instant::now();

        let outcome = run_merge_phase(
            &FailFetchJj,
            &forge,
            &segments,
            &forge.prs,
            &plan.options,
            &plan,
            ForgeKind::GitHub,
            &mut prev_reasons,
            &mut consecutive_errors,
            &mut last_heartbeat,
            &mut state,
            false,
        )
        .expect("run_merge_phase should not error");

        // auth merged once; profile must NOT have been merged.
        assert_eq!(
            forge.merge_calls(),
            vec![1],
            "only auth should merge before the gate fires"
        );
        assert_eq!(outcome.merged.len(), 1);
        assert_eq!(outcome.merged[0].pr_number, 1);

        // Gate uses ReconcileState; outer watch loop reads state.degraded()
        // to decide whether to retry. blocked stays None so the outer loop
        // iterates rather than exits.
        assert!(
            outcome.blocked.is_none(),
            "gate should not return Blocked; G semantics"
        );
        assert!(
            state.degraded(),
            "reconcile failure must mark state as degraded"
        );
        assert!(state.local_failed, "fetch failure is a local-side failure");
        assert!(
            !state.forge_failed,
            "forge side did not fail in this scenario"
        );
        assert!(
            state
                .warnings
                .iter()
                .any(|w| w.kind == DivergenceKind::Local)
        );
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
            let map = prs
                .into_iter()
                .map(|p| (p.head.ref_name.clone(), p))
                .collect();
            Self {
                prs: map,
                merge_calls: std::sync::Mutex::new(vec![]),
            }
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
            Ok(PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".into(),
            })
        }
        fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
            Ok(ChecksStatus::Pass)
        }
        fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary {
                approved_count: 1,
                changes_requested: false,
            })
        }
        fn find_merged_pr(&self, _: &str, _: &str, _: &str) -> Result<Option<PullRequest>> {
            Ok(None)
        }
        fn create_pr(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> {
            Ok(())
        }
        fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> {
            Ok(())
        }
        fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> {
            Ok(vec![])
        }
        fn create_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("test".into())
        }
        fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
            Ok(PrState {
                merged: false,
                state: "open".into(),
            })
        }
    }

    /// Healthy Jj that lets the local-state side of reconcile pass cleanly.
    /// Pairs with ListFailForge to isolate the forge-side failure path.
    struct HealthyJj;
    impl Jj for HealthyJj {
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(vec![])
        }
        fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> {
            Ok(vec![])
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
            Ok(vec![])
        }
        fn get_default_branch(&self) -> Result<String> {
            Ok("main".into())
        }
        fn push_bookmark(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok("wc".into())
        }
        fn rebase_onto(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn merge_into(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> {
            Ok(vec!["c".into()])
        }
        fn is_conflicted(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
    }

    /// Forge serving a non-draft, mergeable, CI-passing PR that has zero
    /// approvals. With required_approvals = 1, evaluate_segment returns
    /// Blocked{InsufficientApprovals} — a segment we must wait on, not merge.
    struct UnapprovedForge {
        prs: HashMap<String, PullRequest>,
    }
    impl UnapprovedForge {
        fn new(prs: Vec<PullRequest>) -> Self {
            let map = prs
                .into_iter()
                .map(|p| (p.head.ref_name.clone(), p))
                .collect();
            Self { prs: map }
        }
    }
    impl Forge for UnapprovedForge {
        fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
            Ok(self.prs.values().cloned().collect())
        }
        fn merge_pr(&self, _: &str, _: &str, _: u64, _: MergeMethod) -> Result<()> {
            panic!("a blocked segment must never be merged");
        }
        fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
            Ok(PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".into(),
            })
        }
        fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
            Ok(ChecksStatus::Pass)
        }
        fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary {
                approved_count: 0,
                changes_requested: false,
            })
        }
        fn find_merged_pr(&self, _: &str, _: &str, _: &str) -> Result<Option<PullRequest>> {
            Ok(None)
        }
        fn create_pr(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> {
            Ok(())
        }
        fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> {
            Ok(())
        }
        fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> {
            Ok(vec![])
        }
        fn create_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("test".into())
        }
        fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
            Ok(PrState {
                merged: false,
                state: "open".into(),
            })
        }
    }

    #[test]
    fn run_merge_phase_flags_waiting_on_blocked_segment() {
        // Regression for issue #4: a segment blocked on an awaited approval
        // must surface as waiting_on_block so the outer loop's no-progress
        // valve treats it as an active wait, not a stall.
        let forge = UnapprovedForge::new(vec![gate_test_pr("auth", 1)]);
        let segments = vec![gate_test_segment("auth")];
        let plan = gate_test_plan();
        let mut state = ReconcileState::default();
        let mut prev_reasons: Option<Vec<BlockReason>> = None;
        let mut consecutive_errors = 0u32;
        let mut last_heartbeat = Instant::now();

        let outcome = run_merge_phase(
            &HealthyJj,
            &forge,
            &segments,
            &forge.prs,
            &plan.options,
            &plan,
            ForgeKind::GitHub,
            &mut prev_reasons,
            &mut consecutive_errors,
            &mut last_heartbeat,
            &mut state,
            false,
        )
        .expect("run_merge_phase should not error");

        assert!(
            outcome.waiting_on_block,
            "blocked segment must set waiting_on_block"
        );
        assert!(
            outcome.merged.is_empty(),
            "nothing should merge while blocked"
        );
        assert!(
            outcome.blocked.is_none(),
            "InsufficientApprovals is a soft wait, not a hard block"
        );
        assert!(
            !outcome.all_done,
            "the phase stopped early to wait, not because it finished"
        );

        // The whole point: is_stalled must not count this as a stall.
        assert!(
            !is_stalled(outcome.waiting_on_block, 0, 0, false),
            "an active wait must not trip the no-progress valve",
        );
    }

    // The contrast with the test above: a native stack never clears, so it must
    // come back as a hard `blocked`, which is what makes the outer watch loop
    // break immediately instead of polling. Returning it as a soft wait would
    // spin at the poll interval forever.
    #[test]
    fn run_merge_phase_hard_blocks_on_a_native_stack() {
        let mut pr = gate_test_pr("auth", 1);
        pr.stack = Some(crate::forge::types::PrStackRef {
            number: 223,
            id: 1,
            position: 2,
            size: 4,
            base: Some(PullRequestRef {
                ref_name: "main".into(),
                label: String::new(),
                sha: String::new(),
            }),
        });
        let forge = UnapprovedForge::new(vec![pr]);
        let segments = vec![gate_test_segment("auth")];
        let plan = gate_test_plan();
        let mut state = ReconcileState::default();
        let mut prev_reasons: Option<Vec<BlockReason>> = None;
        let mut consecutive_errors = 0u32;
        let mut last_heartbeat = Instant::now();

        let outcome = run_merge_phase(
            &HealthyJj,
            &forge,
            &segments,
            &forge.prs,
            &plan.options,
            &plan,
            ForgeKind::GitHub,
            &mut prev_reasons,
            &mut consecutive_errors,
            &mut last_heartbeat,
            &mut state,
            false,
        )
        .expect("run_merge_phase should not error");

        let blocked = outcome
            .blocked
            .expect("a native stack must be a hard block");
        assert!(matches!(
            blocked.reasons[..],
            [BlockReason::NativeStack {
                stack_number: 223,
                ..
            }]
        ));
        assert_eq!(
            blocked.pr_number,
            Some(1),
            "the blocked PR should be identified"
        );
        assert!(
            !outcome.waiting_on_block,
            "must not be reported as an active wait; that would keep the loop polling",
        );
        assert!(outcome.merged.is_empty(), "nothing should merge");
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
            native_stack_block: None,
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
            native_stack_block: None,
            warnings: vec![LocalDivergenceWarning {
                kind: DivergenceKind::Forge,
                message: "list_open_prs failed".into(),
            }],
        }
    }

    const HEARTBEAT: Duration = Duration::from_secs(60);

    #[test]
    fn classify_clean_state_no_prev_is_continue() {
        let action = classify_post_merge(&empty_state(), &None, Duration::ZERO, HEARTBEAT);
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
        let action = classify_post_merge(&local_failure_state(), &None, Duration::ZERO, HEARTBEAT);
        assert_eq!(action, PostMergeAction::NewFailure);
        assert!(action.waits());
    }

    #[test]
    fn classify_persistent_same_failure_before_heartbeat_is_dot() {
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let action = classify_post_merge(
            &local_failure_state(),
            &prev,
            Duration::from_secs(10),
            HEARTBEAT,
        );
        assert_eq!(action, PostMergeAction::Quiet);
        assert!(action.waits());
    }

    #[test]
    fn classify_persistent_same_failure_after_heartbeat_is_heartbeat() {
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let action = classify_post_merge(
            &local_failure_state(),
            &prev,
            Duration::from_secs(120),
            HEARTBEAT,
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
            native_stack_block: None,
            warnings: vec![],
        };
        let action = classify_post_merge(&mixed, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(action, PostMergeAction::NewFailure);
    }

    #[test]
    fn classify_local_to_forge_only_is_new_failure() {
        let prev = Some(vec![BlockReason::LocalSyncFailed]);
        let action = classify_post_merge(&forge_failure_state(), &prev, Duration::ZERO, HEARTBEAT);
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
        assert_eq!(
            a2,
            PostMergeAction::Quiet,
            "iter 2 within heartbeat window must be quiet"
        );
        // prev unchanged

        // Iter 3: same failure, 65s elapsed → Heartbeat.
        let a3 = classify_post_merge(&degraded, &prev, Duration::from_secs(65), HEARTBEAT);
        assert_eq!(
            a3,
            PostMergeAction::Heartbeat,
            "iter 3 past heartbeat must surface"
        );
        // loop resets last_heartbeat when heartbeat fires

        // Iter 4: user fixed it → state clean → Recovered.
        let clean = empty_state();
        let a4 = classify_post_merge(&clean, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(
            a4,
            PostMergeAction::Recovered,
            "iter 4 must announce recovery"
        );
        prev = None;

        // Iter 5: still clean, prev cleared → Continue.
        let a5 = classify_post_merge(&clean, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(
            a5,
            PostMergeAction::Continue,
            "iter 5 returns to silent steady-state"
        );

        // Iter 6: a fresh failure (different kind this time) → NewFailure.
        // Verifies we re-announce instead of staying silent.
        let new_failure = forge_failure_state();
        let a6 = classify_post_merge(&new_failure, &prev, Duration::ZERO, HEARTBEAT);
        assert_eq!(
            a6,
            PostMergeAction::NewFailure,
            "fresh failure must reannounce"
        );
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
            &local_failure_state(),
            &prev,
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(a, PostMergeAction::Heartbeat);
    }

    #[test]
    fn run_merge_phase_gates_after_forge_reconcile_failure() {
        // Mirrors the local-failure test but on the forge side: local
        // reconcile passes cleanly, then forge-state reconcile fails on
        // list_open_prs. The gate must still fire and the state must
        // tag warnings as Forge-kind so users get the right recovery hint.
        let forge = ListFailForge::new(vec![gate_test_pr("auth", 1), gate_test_pr("profile", 2)]);
        let segments = vec![gate_test_segment("auth"), gate_test_segment("profile")];
        let plan = gate_test_plan();
        let mut state = ReconcileState::default();
        let mut prev_reasons: Option<Vec<BlockReason>> = None;
        let mut consecutive_errors = 0u32;
        let mut last_heartbeat = Instant::now();

        let outcome = run_merge_phase(
            &HealthyJj,
            &forge,
            &segments,
            &forge.prs,
            &plan.options,
            &plan,
            ForgeKind::GitHub,
            &mut prev_reasons,
            &mut consecutive_errors,
            &mut last_heartbeat,
            &mut state,
            false,
        )
        .expect("run_merge_phase should not error");

        assert_eq!(forge.merge_calls(), vec![1], "only auth should merge");
        assert!(outcome.blocked.is_none(), "gate keeps watch iterating");
        assert!(state.degraded());
        assert!(!state.local_failed, "local side was healthy");
        assert!(
            state.forge_failed,
            "list_open_prs failure must set forge_failed"
        );
        assert!(
            state
                .warnings
                .iter()
                .any(|w| w.kind == DivergenceKind::Forge),
            "must record a Forge-kind warning"
        );
    }

    // --- "commit switched during watch": what rediscover_segments does when the
    // watched stack changes mid-watch. Real jj, since it exercises the whole
    // graph-build -> analyze -> infer path. ---

    fn jj_installed() -> bool {
        std::process::Command::new("jj")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Two independent stacks off master — bookmark `bA` and bookmark `bB` —
    /// with `@` left on `bB`. Returns the tempdir (keep it alive) and a runner.
    fn two_stack_repo() -> (tempfile::TempDir, crate::jj::JjRunner) {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = ["--config=user.name=T", "--config=user.email=t@e.com"];
        let jj = |args: &[&str]| {
            let mut full: Vec<&str> = cfg.to_vec();
            full.extend_from_slice(args);
            std::process::Command::new("jj")
                .args(&full)
                .current_dir(dir.path())
                .output()
                .expect("jj");
        };
        std::process::Command::new("jj")
            .args(["git", "init"])
            .current_dir(dir.path())
            .output()
            .expect("init");
        for (k, v) in [("user.name", "T"), ("user.email", "t@e.com")] {
            jj(&["config", "set", "--repo", k, v]);
        }
        std::fs::write(dir.path().join("base.txt"), "b\n").unwrap();
        jj(&["describe", "-m", "BASE"]);
        jj(&["bookmark", "create", "master", "-r", "@"]);
        jj(&["new", "master", "-m", "A"]);
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        jj(&["status"]);
        jj(&["bookmark", "create", "bA", "-r", "@"]);
        jj(&["new", "master", "-m", "B"]);
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        jj(&["status"]);
        jj(&["bookmark", "create", "bB", "-r", "@"]);
        let runner = crate::jj::JjRunner::new(dir.path().to_path_buf()).unwrap();
        (dir, runner)
    }

    fn segment_bookmarks(segs: &[NarrowedSegment]) -> Vec<String> {
        segs.iter().map(|s| s.bookmark.name.clone()).collect()
    }

    #[test]
    fn rediscover_follows_target_bookmark_not_the_working_copy() {
        if !jj_installed() {
            return;
        }
        let (_dir, jj) = two_stack_repo();
        // `@` is on bB, but we're watching bA — rediscover must follow the
        // target bookmark, not the working copy. (This is why moving `@`
        // mid-watch doesn't hijack the watch.)
        let names = segment_bookmarks(&rediscover_segments(&jj, "bA").unwrap());
        assert!(
            names.contains(&"bA".to_string()),
            "should follow target bA; got {names:?}"
        );
        assert!(
            !names.contains(&"bB".to_string()),
            "must not pick up @'s stack bB; got {names:?}"
        );
    }

    #[test]
    fn rediscover_stops_instead_of_hijacking_when_target_gone_and_wc_elsewhere() {
        if !jj_installed() {
            return;
        }
        let (dir, jj) = two_stack_repo();
        // The watched bookmark disappears mid-watch (renamed/abandoned in another
        // window)...
        std::process::Command::new("jj")
            .args([
                "--config=user.name=T",
                "--config=user.email=t@e.com",
                "bookmark",
                "delete",
                "bA",
            ])
            .current_dir(dir.path())
            .output()
            .expect("delete bA");
        // ...and `@` happens to be on a DIFFERENT stack (bB). We must NOT pivot
        // to bB — that would auto-merge a stack the user never asked to watch.
        // The target is gone, so rediscover returns empty and the loop stops.
        let names = segment_bookmarks(&rediscover_segments(&jj, "bA").unwrap());
        assert!(
            names.is_empty(),
            "gone target must stop, not hijack @'s stack; got {names:?}"
        );
    }

    fn jj_run(dir: &std::path::Path, args: &[&str]) {
        let mut full = vec!["--config=user.name=T", "--config=user.email=t@e.com"];
        full.extend_from_slice(args);
        std::process::Command::new("jj")
            .args(&full)
            .current_dir(dir)
            .output()
            .expect("jj");
    }
    fn jj_out(dir: &std::path::Path, args: &[&str]) -> String {
        let mut full = vec!["--config=user.name=T", "--config=user.email=t@e.com"];
        full.extend_from_slice(args);
        let o = std::process::Command::new("jj")
            .args(&full)
            .current_dir(dir)
            .output()
            .expect("jj");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    #[test]
    fn rediscover_follows_target_across_a_rebase() {
        if !jj_installed() {
            return;
        }
        let (dir, jj) = two_stack_repo();
        let before = jj_out(
            dir.path(),
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "bA",
                "--no-graph",
                "-T",
                "commit_id",
            ],
        );
        // Rebase bA onto bB: bA's commit id changes, but jj moves the bookmark
        // NAME with it. This is why ordinary VCS churn (the reconcile rebase, a
        // concurrent rebase) never triggers the fallback — the target stays
        // findable by name. The infer fallback is NOT the mechanism that "keeps
        // track after a move".
        jj_run(
            dir.path(),
            &["--ignore-working-copy", "rebase", "-r", "bA", "-d", "bB"],
        );
        let after = jj_out(
            dir.path(),
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "bA",
                "--no-graph",
                "-T",
                "commit_id",
            ],
        );
        assert_ne!(
            before, after,
            "the rebase should have changed bA's commit id"
        );

        let names = segment_bookmarks(&rediscover_segments(&jj, "bA").unwrap());
        assert!(
            names.contains(&"bA".to_string()),
            "target must stay findable after a rebase; got {names:?}"
        );
    }

    #[test]
    fn rediscover_stops_when_target_gone_and_working_copy_is_off_any_stack() {
        if !jj_installed() {
            return;
        }
        let (dir, jj) = two_stack_repo();
        // Move @ onto trunk (off both stacks), then the watched bookmark vanishes.
        jj_run(dir.path(), &["edit", "master"]);
        jj_run(dir.path(), &["bookmark", "delete", "bA"]);
        // Target gone AND @ is on nothing watchable -> rediscover returns empty,
        // which the loop routes into "report orphaned PRs and stop". This is the
        // benign outcome of the same fallback that misbehaves in the test above:
        // whether it stops or hijacks depends entirely on where @ happens to be.
        let names = segment_bookmarks(&rediscover_segments(&jj, "bA").unwrap());
        assert!(
            names.is_empty(),
            "should stop (empty) when target gone and @ is off any stack; got {names:?}"
        );
    }

    #[test]
    fn rediscover_keeps_target_findable_through_a_bottom_squash_merge() {
        if !jj_installed() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let d = dir.path();
        std::process::Command::new("jj")
            .args(["git", "init"])
            .current_dir(d)
            .output()
            .expect("init");
        jj_run(d, &["config", "set", "--repo", "user.name", "T"]);
        jj_run(d, &["config", "set", "--repo", "user.email", "t@e.com"]);
        std::fs::write(d.join("base.txt"), "b\n").unwrap();
        jj_run(d, &["describe", "-m", "BASE"]);
        jj_run(d, &["bookmark", "create", "master", "-r", "@"]);
        // Linear stack: bottom (bBot) -> top (bTop), @ on top.
        jj_run(d, &["new", "-m", "BOTTOM"]);
        std::fs::write(d.join("bot.txt"), "x\n").unwrap();
        jj_run(d, &["status"]);
        jj_run(d, &["bookmark", "create", "bBot", "-r", "@"]);
        jj_run(d, &["new", "-m", "TOP"]);
        std::fs::write(d.join("top.txt"), "y\n").unwrap();
        jj_run(d, &["status"]);
        jj_run(d, &["bookmark", "create", "bTop", "-r", "@"]);
        // The bottom PR squash-merges: a squash commit lands on trunk and trunk
        // advances to it (as `jj git fetch` would import). The local stack still
        // hangs off the old base until jjpr reconciles on the next poll.
        jj_run(
            d,
            &[
                "--ignore-working-copy",
                "new",
                "--no-edit",
                "master",
                "-m",
                "SQUASH",
            ],
        );
        let s = jj_out(
            d,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "description(\"SQUASH\")",
                "--no-graph",
                "-T",
                "commit_id",
            ],
        );
        jj_run(
            d,
            &[
                "--ignore-working-copy",
                "bookmark",
                "set",
                "master",
                "-r",
                &s,
            ],
        );

        let jj = crate::jj::JjRunner::new(d.to_path_buf()).unwrap();
        // The watched target (top) stays findable THROUGH the bottom squash
        // merge — it never becomes unfindable, so the @-fallback never triggers.
        // This is what makes it safe to drop the hijacking fallback.
        let names = segment_bookmarks(&rediscover_segments(&jj, "bTop").unwrap());
        assert!(
            names.contains(&"bTop".to_string()),
            "target must stay findable through a bottom squash-merge; got {names:?}"
        );
    }

    #[test]
    fn rediscover_when_target_commit_is_conflicted() {
        if !jj_installed() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let d = dir.path();
        std::process::Command::new("jj")
            .args(["git", "init"])
            .current_dir(d)
            .output()
            .expect("init");
        jj_run(d, &["config", "set", "--repo", "user.name", "T"]);
        jj_run(d, &["config", "set", "--repo", "user.email", "t@e.com"]);
        std::fs::write(d.join("f.txt"), "LIMIT = 10\n").unwrap();
        jj_run(d, &["describe", "-m", "BASE"]);
        jj_run(d, &["bookmark", "create", "master", "-r", "@"]);
        // `base` stands in for the squashed bottom's content now on the base the
        // top gets rebased onto: it edits the same line the top will.
        jj_run(d, &["new", "master", "-m", "NEWBASE"]);
        std::fs::write(d.join("f.txt"), "LIMIT = 20\n").unwrap();
        jj_run(d, &["status"]);
        jj_run(d, &["bookmark", "create", "bBase", "-r", "@"]);
        // The top edits the same line, then gets rebased onto that base — both
        // changed LIMIT, so the top lands CONFLICTED. This is the end state a
        // squash-merge reconcile produces when the top overlaps the bottom.
        jj_run(d, &["new", "master", "-m", "TOP"]);
        std::fs::write(d.join("f.txt"), "LIMIT = 30\n").unwrap();
        jj_run(d, &["status"]);
        jj_run(d, &["bookmark", "create", "bTop", "-r", "@"]);
        jj_run(d, &["rebase", "-s", "bTop", "-d", "bBase"]);
        let conflicted = jj_out(
            d,
            &[
                "--ignore-working-copy",
                "log",
                "-r",
                "bTop",
                "--no-graph",
                "-T",
                "if(conflict, \"yes\", \"no\")",
            ],
        );
        assert_eq!(conflicted, "yes", "test setup: bTop should be conflicted");

        let jj = crate::jj::JjRunner::new(d.to_path_buf()).unwrap();
        let segs = rediscover_segments(&jj, "bTop").unwrap();
        let names = segment_bookmarks(&segs);
        // A CONTENT-conflicted commit is NOT dropped from the graph (the
        // "skipping" path is for divergent/missing bookmarks, not conflict
        // markers), so the target stays findable. That means it never falls
        // through to the @-inference fallback, and it never prematurely stops —
        // it flows into the loop's conflict-wait instead.
        assert!(
            names.contains(&"bTop".to_string()),
            "a conflicted target must stay findable so the conflict-wait handles it; got {names:?}"
        );
        // And the conflict flag reaches the segment, so Phase 1b actually waits
        // for resolution rather than proceeding.
        assert!(
            segs.iter().any(|s| s.changes.iter().any(|c| c.conflict)),
            "the conflicted target's segment must carry the conflict flag so the loop waits for resolution"
        );
    }
}
