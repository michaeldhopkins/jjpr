use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::forge::comment;
use crate::forge::http::HttpError;
use crate::forge::types::{MergeMethod, PullRequest};
use crate::forge::{Forge, ForgeKind};
use crate::jj::types::NarrowedSegment;
use crate::jj::Jj;

use super::plan::{evaluate_segment, BlockReason, MergePlan, PrMergeStatus};

/// jj's corruption signal — divergent change ids.
enum Divergence {
    Clean,
    Present(Vec<String>),
}

/// Read the divergence signal, failing SAFE. A read error here is almost always
/// lock contention from the very concurrent writer we're guarding against (jj's
/// op-heads lock; `run_jj` does not retry), and must NEVER be mistaken for
/// "clean" — that would let a mangled tree through the gate. So an error is
/// reported as `Present` (with no ids to name), gating the caller.
fn divergence(jj: &dyn Jj) -> Divergence {
    match jj.divergent_change_ids() {
        Ok(d) if d.is_empty() => Divergence::Clean,
        Ok(d) => Divergence::Present(d),
        Err(_) => Divergence::Present(Vec::new()),
    }
}

/// Attempt to synchronize local state after a forge merge.
///
/// Returns warnings for any local failures (fetch, divergence, rebase, push)
/// instead of propagating errors. An empty vec means full success.
fn reconcile_local_state(
    jj: &dyn Jj,
    segments: &[NarrowedSegment],
    seg_idx: usize,
    effective_base: &str,
    remote_name: &str,
    strategy: crate::config::ReconcileStrategy,
) -> Vec<LocalDivergenceWarning> {
    let mut warnings = Vec::new();

    let mk = |message: String| LocalDivergenceWarning {
        kind: DivergenceKind::Local,
        message,
    };

    // The corruption signal is jj's first-class divergent() set: a concurrent
    // op-log reconcile — or a rebase racing one — leaves two versions of a
    // change. That is the only thing we gate on. A concurrent reconcile that does
    // NOT diverge is independent work and is safe to proceed through (proven:
    // tests/recovery_scenarios.rs::proceeding_through_a_nondivergent_reconcile_stays_clean).
    //
    // If the stack is ALREADY divergent, do not rebase it — rebasing a divergent
    // stack is what collapses/drops files. Both versions are preserved in place;
    // gate and surface, and the next poll retries once it's resolved. We never
    // roll back past the divergence, so no real work is discarded.
    if let Divergence::Present(ids) = divergence(jj) {
        return vec![concurrent_gate_warning(false, &ids)];
    }

    println!("  Fetching remotes...");
    if let Err(e) = jj.git_fetch() {
        // A failed fetch can still have reconciled a concurrent fork into a
        // divergent state; surface it.
        if let Divergence::Present(ids) = divergence(jj) {
            return vec![concurrent_gate_warning(false, &ids)];
        }
        warnings.push(mk(format!("Failed to fetch remotes: {e}")));
        return warnings;
    }

    // The operation right after fetch — the clean, work-preserving point to roll
    // a bad rebase back to. We NEVER roll back past this: rolling back to before
    // the fetch would discard the concurrent process's work, and jj already
    // preserves both sides' commits in the reconciled state.
    let post_fetch_op = jj.current_operation_id().ok().filter(|s| !s.is_empty());

    // A concurrent reconcile during the fetch that left the stack divergent: our
    // work and the other process's work are both present but divergent. Do NOT
    // rebase on top of that — that is the mangle. Gate, preserving both, retry.
    if let Divergence::Present(ids) = divergence(jj) {
        return vec![concurrent_gate_warning(false, &ids)];
    }

    // Track which bookmarks to push. With merge strategy, only push bookmarks
    // whose merge_into succeeded and are conflict-free.
    let bookmarks_to_push: Vec<&str> = match strategy {
        crate::config::ReconcileStrategy::Merge => {
            // Merge-based sync: create merge commits incorporating the new
            // base. This is append-only; pushes are fast-forward, no force.
            println!("  Syncing remaining stack with {effective_base}...");
            let mut succeeded = Vec::new();
            for seg in &segments[seg_idx + 1..] {
                if let Err(e) = jj.merge_into(&seg.bookmark.name, effective_base) {
                    warnings.push(mk(format!(
                        "Failed to merge-sync '{}': {e}",
                        seg.bookmark.name
                    )));
                    break;
                }
                // jj creates the merge commit even with conflicts; check before pushing.
                match jj.is_conflicted(&seg.bookmark.name) {
                    Ok(true) => {
                        warnings.push(mk(format!(
                            "Merge of '{effective_base}' into '{}' has conflicts; skipping push",
                            seg.bookmark.name
                        )));
                        break;
                    }
                    Err(e) => {
                        warnings.push(mk(format!(
                            "Could not check conflict state of '{}': {e}",
                            seg.bookmark.name
                        )));
                        break;
                    }
                    Ok(false) => {
                        succeeded.push(seg.bookmark.name.as_str());
                    }
                }
            }
            succeeded
        }
        crate::config::ReconcileStrategy::Rebase => {
            let next_segment = &segments[seg_idx + 1];
            let next_change_id = &next_segment.bookmark.change_id;

            match jj.resolve_change_id(next_change_id) {
                Ok(ref commit_ids) if commit_ids.len() > 1 => {
                    let short_id = &next_change_id[..next_change_id.len().min(12)];
                    let count = commit_ids.len();
                    warnings.push(mk(format!(
                        "Change '{short_id}' is divergent ({count} commits share this change ID)"
                    )));
                    return warnings;
                }
                Ok(commit_ids) if commit_ids.is_empty() => {
                    warnings.push(mk(format!(
                        "Change ID '{next_change_id}' not found locally"
                    )));
                    return warnings;
                }
                Err(_) => {}
                _ => {}
            }

            // Rebase from the oldest commit in the next segment, not the bookmark tip.
            let root = rebase_root(next_segment);

            println!("  Rebasing remaining stack onto {effective_base}...");
            if let Err(e) = jj.rebase_onto(root, effective_base) {
                warnings.push(mk(format!("Failed to rebase remaining stack: {e}")));
                return warnings;
            }

            // Rebase succeeded; push all remaining bookmarks.
            segments[seg_idx + 1..]
                .iter()
                .map(|s| s.bookmark.name.as_str())
                .collect()
        }
    };

    // If the rebase introduced divergence — it raced a concurrent reconcile, or
    // operated on a state that mangled — undo ONLY the rebase: restore to the
    // clean post-fetch op (which preserves both our work and the fetched changes)
    // and retry. Never publish a mangled tree, and never roll back past the fetch.
    if let Divergence::Present(ids) = divergence(jj) {
        return match post_fetch_op.as_deref() {
            // Restore succeeded — the tree is clean again.
            Some(op) if jj.restore_operation(op).is_ok() => {
                vec![concurrent_gate_warning(true, &[])]
            }
            // Restore failed (likely the same lock contention) or we never
            // captured a post-fetch op: the divergent tree is still checked out,
            // so tell the truth. It is not pushed, and the next poll re-gates.
            _ => vec![concurrent_gate_warning(false, &ids)],
        };
    }

    for name in &bookmarks_to_push {
        println!("  Pushing '{name}'...");
        if let Err(e) = jj.push_bookmark(name, remote_name) {
            warnings.push(mk(format!("Failed to push '{name}': {e}")));
            break;
        }
    }

    warnings
}

/// A work-preserving concurrent-modification warning. jj's reconcile keeps both
/// sides' commits, so recovery never discards work: we either gate before the
/// mangling rebase (`restored = false`) or roll only the rebase back to the
/// clean post-fetch op (`restored = true`). No "run jj op restore" hand-off.
fn concurrent_gate_warning(restored: bool, divergent_ids: &[String]) -> LocalDivergenceWarning {
    let mut message = if restored {
        "Paused: a concurrent jj process raced jjpr's restack. jjpr rolled its \
         in-progress restack back to the clean fetched state — your work and the \
         fetched changes are intact — and will retry on the next poll."
            .to_string()
    } else {
        "Paused: a concurrent jj process modified the operation log while jjpr was \
         reconciling. Both your work and the other process's work are preserved; \
         jjpr did not restack, to avoid corrupting the stack, and will retry on \
         the next poll."
            .to_string()
    };
    if !divergent_ids.is_empty() {
        message.push_str(&format!(
            " The stack has a divergent change ({}) — two versions of the same \
             change from the concurrent modification, both kept. jjpr continues \
             once it is resolved (keep one with `jj abandon <the-stale-commit>`).",
            divergent_ids.join(", ")
        ));
    }
    message.push_str(" If another jj/jjpr process is running on this repo, pause it.");
    LocalDivergenceWarning {
        kind: DivergenceKind::Concurrent,
        message,
    }
}

/// Refresh PR state from forge and retarget the next PR's base if needed.
///
/// Independent of local state — runs even when local reconciliation failed.
/// Returns `(Option<fresh_map>, Vec<warnings>)` — never errors, since the
/// forge merge already happened and reconciliation is best-effort.
fn reconcile_forge_state(
    forge: &dyn Forge,
    nav: &dyn comment::StackNav,
    segments: &[NarrowedSegment],
    seg_idx: usize,
    owner: &str,
    repo: &str,
    effective_base: &str,
    fk: ForgeKind,
) -> (Option<HashMap<String, PullRequest>>, Vec<LocalDivergenceWarning>) {
    let mut warnings = Vec::new();
    let mk = |message: String| LocalDivergenceWarning {
        kind: DivergenceKind::Forge,
        message,
    };

    let fresh_prs = match forge.list_open_prs(owner, repo) {
        Ok(prs) => prs,
        Err(e) => {
            warnings.push(mk(format!("Failed to refresh PR list: {e}")));
            return (None, warnings);
        }
    };
    let fresh_map = crate::forge::build_pr_map(fresh_prs, owner);

    let next_name = &segments[seg_idx + 1].bookmark.name;
    if let Some(next_pr) = fresh_map.get(next_name)
        && next_pr.base.ref_name != effective_base
    {
        println!(
            "  Updating {} base to '{effective_base}'...",
            fk.format_ref(next_pr.number)
        );
        if let Err(e) = forge.update_pr_base(owner, repo, next_pr.number, effective_base) {
            warnings.push(mk(format!(
                "Failed to retarget {} base to '{effective_base}': {e}",
                fk.format_ref(next_pr.number)
            )));
        }
    }

    // Update stack nav on remaining open PRs to mark resolved segments.
    let merged_names: std::collections::HashSet<&str> = segments[..=seg_idx]
        .iter()
        .map(|s| s.bookmark.name.as_str())
        .collect();

    for seg in &segments[seg_idx + 1..] {
        let Some(pr) = fresh_map.get(&seg.bookmark.name) else {
            continue;
        };
        let seg_name = seg.bookmark.name.clone();
        let result = nav.update(forge, owner, repo, pr, &|previous_data| {
            let Some(data) = previous_data else {
                return (vec![], vec![]);
            };
            partition_after_merge(&data.stack, &merged_names, &seg_name)
        });
        if let Err(e) = result {
            warnings.push(mk(format!(
                "Failed to update stack nav on {}: {e}",
                fk.format_ref(pr.number)
            )));
        }
    }

    (Some(fresh_map), warnings)
}

/// Split a stack-info comment's previous payload into `(live, fossils)`
/// after a forge merge. Items in `merged_names` move to fossils;
/// previously-merged items stay as fossils; live items keep their stored
/// metadata. Newly-merged items inherit `closed_at: None` until the next
/// `jjpr submit` queries the forge to populate the real merge timestamp.
///
/// Order matches the input: live entries follow stack position; fossils
/// preserve the original comment's order, which under jjpr's existing
/// rendering means just-merged entries (formerly live, listed first in
/// `data.stack`) naturally appear above older fossils — matching the
/// "most recent first" sort that the next submit will produce.
fn partition_after_merge(
    items: &[comment::StackCommentItem],
    merged_names: &std::collections::HashSet<&str>,
    current_seg_name: &str,
) -> (Vec<comment::StackEntry>, Vec<comment::StackEntry>) {
    let mut live = Vec::new();
    let mut fossils = Vec::new();
    for item in items {
        let is_merged =
            item.is_merged || merged_names.contains(item.bookmark_name.as_str());
        let entry = comment::StackEntry {
            bookmark_name: item.bookmark_name.clone(),
            pr_url: Some(item.pr_url.clone()),
            pr_number: Some(item.pr_number),
            is_current: item.bookmark_name == current_seg_name && !is_merged,
            is_merged,
            closed_at: item.closed_at.clone(),
        };
        if is_merged {
            fossils.push(entry);
        } else {
            live.push(entry);
        }
    }
    (live, fossils)
}

/// Run both local and forge reconciliation after a successful merge.
///
/// Best-effort: the forge merge already happened, so failures here are
/// reported as warnings on `state` rather than propagated. Each kind of
/// failure (local sync vs. forge reconcile) sets a separate flag on the
/// state so the gate can emit the right BlockReason and the user can
/// see the right recovery hints.
pub(crate) fn reconcile_after_merge(
    jj: &dyn Jj,
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    seg_idx: usize,
    plan: &MergePlan,
    fk: ForgeKind,
    state: &mut ReconcileState,
) -> Option<HashMap<String, PullRequest>> {
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;
    let effective_base = plan.stack_base.as_deref().unwrap_or(&plan.default_branch);

    // Invariant: callers gate further merges on state.degraded() and
    // either break the loop (execute_merge_plan / run_merge_phase) or
    // call state.reset() at the next iteration (run_watch_loop). So
    // reconcile_after_merge should never be re-entered with local_failed
    // already set. The previous "Skipping local sync (local state
    // already diverged)" branch is dead under that invariant.
    debug_assert!(
        !state.degraded(),
        "reconcile_after_merge re-entered with a degraded state; \
         caller forgot to gate or reset state"
    );
    let warnings = reconcile_local_state(
        jj, segments, seg_idx, effective_base, &plan.remote_name,
        plan.options.reconcile_strategy,
    );
    // Ordinary local-sync failures need a manual rebase (local_failed). A
    // concurrent op-log reconcile was handled work-preservingly (Concurrent
    // warning) and degrades via has_concurrent(); both gate further merges.
    if warnings.iter().any(|w| w.kind == DivergenceKind::Local) {
        state.local_failed = true;
    }
    state.warnings.extend(warnings);

    let nav = comment::create_stack_nav(plan.stack_nav);
    let (fresh_map, forge_warnings) =
        reconcile_forge_state(forge, nav.as_ref(), segments, seg_idx, owner, repo, effective_base, fk);
    if !forge_warnings.is_empty() {
        state.forge_failed = true;
        state.warnings.extend(forge_warnings);
    }
    fresh_map
}

/// The change ID to hand to `jj rebase -s ...` to rebase the entire
/// commit chain inside a segment. For multi-commit segments, this is
/// the OLDEST commit (changes.last() in jj's newest-first ordering),
/// not the bookmark tip. Using the bookmark's change_id alone would
/// rebase only the tip and strand the older commits under the old base.
pub fn rebase_root(segment: &NarrowedSegment) -> &str {
    segment
        .changes
        .last()
        .map(|c| c.change_id.as_str())
        .unwrap_or(&segment.bookmark.change_id)
}

/// If reconcile produced any failures, construct a BlockedPr for the
/// next segment and print the block message. Returns None when state
/// is clean. All three merge call sites use this so the gate semantics
/// stay identical and the print format stays consistent.
pub(crate) fn gate_after_reconcile(
    state: &ReconcileState,
    next: &NarrowedSegment,
    pr_map: Option<&HashMap<String, PullRequest>>,
    fk: ForgeKind,
) -> Option<BlockedPr> {
    if !state.degraded() {
        return None;
    }
    let next_pr_number = pr_map
        .and_then(|m| m.get(&next.bookmark.name))
        .map(|p| p.number);
    let pr_label = next_pr_number
        .map(|n| format!(" ({})", fk.format_ref(n)))
        .unwrap_or_default();
    let reasons = state.block_reasons();
    println!("  Blocked at '{}'{pr_label}:", next.bookmark.name);
    for reason in &reasons {
        println!("    - {}", format_block_reason(reason, fk));
    }
    Some(BlockedPr {
        bookmark_name: next.bookmark.name.clone(),
        pr_number: next_pr_number,
        reasons,
    })
}

/// A PR that was successfully merged.
#[derive(Debug)]
pub struct MergedPr {
    pub bookmark_name: String,
    pub pr_number: u64,
    pub html_url: String,
}

/// A PR that blocked further merging.
#[derive(Debug)]
pub struct BlockedPr {
    pub bookmark_name: String,
    pub pr_number: Option<u64>,
    pub reasons: Vec<BlockReason>,
}

/// A PR that was already merged before we ran.
#[derive(Debug)]
pub struct SkippedMergedPr {
    pub bookmark_name: String,
    pub pr_number: u64,
}

/// Whether a reconcile warning came from local repo sync or from the
/// forge-side reconcile pass. Drives recovery hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DivergenceKind {
    /// `reconcile_local_state` failure: fetch, rebase, merge_into, push,
    /// divergent change ID, missing change ID, or local conflict.
    Local,
    /// `reconcile_forge_state` failure: list_open_prs, update_pr_base,
    /// or stack-comment update.
    Forge,
    /// A concurrent jj process reconciled the op log mid-reconcile; we paused
    /// before the mangling rebase (preserving both sides' work) and retry.
    /// Recovery is automatic and work-preserving, not a manual fix — so it
    /// renders differently from `Local`.
    Concurrent,
}

/// A warning recorded during reconcile_after_merge.
#[derive(Debug, Clone)]
pub struct LocalDivergenceWarning {
    pub kind: DivergenceKind,
    pub message: String,
}

/// Carrying state for reconcile_after_merge. Tracks whether each pass
/// failed and accumulates warning text. The gate consults `degraded()`
/// after each reconcile call to decide whether to stop merging.
#[derive(Debug, Default)]
pub struct ReconcileState {
    pub local_failed: bool,
    pub forge_failed: bool,
    pub warnings: Vec<LocalDivergenceWarning>,
}

impl ReconcileState {
    pub fn degraded(&self) -> bool {
        self.local_failed || self.forge_failed || self.has_concurrent()
    }

    /// Whether this pass hit a concurrent op-log reconcile (and paused/rolled
    /// only the rebase back to preserve work). Derived from the warnings so the
    /// struct stays a plain flag/warning bag.
    pub fn has_concurrent(&self) -> bool {
        self.warnings.iter().any(|w| w.kind == DivergenceKind::Concurrent)
    }

    /// Block reasons corresponding to the current failure flags. Returns
    /// an empty vec when the state is clean.
    pub fn block_reasons(&self) -> Vec<BlockReason> {
        let mut reasons = Vec::new();
        if self.has_concurrent() {
            reasons.push(BlockReason::ConcurrentModification);
        }
        if self.local_failed {
            reasons.push(BlockReason::LocalSyncFailed);
        }
        if self.forge_failed {
            reasons.push(BlockReason::ForgeReconcileFailed);
        }
        reasons
    }

    /// Wipe failure state so a follow-up reconcile gets a fresh chance.
    /// Used by run_watch_loop between iterations so the user can fix
    /// local state and have watch resume on the next poll.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Result of executing a merge plan.
#[derive(Debug)]
pub struct MergeResult {
    pub merged: Vec<MergedPr>,
    pub blocked_at: Option<BlockedPr>,
    pub skipped_merged: Vec<SkippedMergedPr>,
    pub local_warnings: Vec<LocalDivergenceWarning>,
}

/// Execute the merge plan: merge PRs, fetch, rebase, push, retarget bases.
///
/// After each successful merge, re-evaluates remaining segments against
/// live GitHub state rather than trusting the upfront plan.
pub fn execute_merge_plan(
    jj: &dyn Jj,
    github: &dyn Forge,
    plan: &MergePlan,
    segments: &[NarrowedSegment],
    dry_run: bool,
) -> Result<MergeResult> {
    if dry_run {
        return execute_dry_run(plan);
    }

    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;
    let fk = plan.forge_kind;

    let mut merged = Vec::new();
    let mut blocked_at = None;
    let mut skipped_merged = Vec::new();
    let mut state = ReconcileState::default();

    // Always evaluate segments just-in-time against fresh forge state.
    // The upfront plan.actions are only used for dry_run display.
    let fresh_prs = github.list_open_prs(owner, repo)?;
    let mut pr_map: Option<HashMap<String, PullRequest>> = Some(
        crate::forge::build_pr_map(fresh_prs, owner),
    );

    for (seg_idx, segment) in segments.iter().enumerate() {
        let status = if let Some(ref map) = pr_map {
            evaluate_segment(
                github,
                &segment.bookmark.name,
                &plan.repo_info,
                map,
                &plan.options,
            )?
        } else if let Some(action) = plan.actions.get(seg_idx) {
            action.clone()
        } else {
            break;
        };

        let needs_reconcile = match status {
            PrMergeStatus::AlreadyMerged {
                bookmark_name,
                pr_number,
            } => {
                println!(
                    "  Skipping '{bookmark_name}': {} already merged",
                    fk.format_ref(pr_number)
                );
                skipped_merged.push(SkippedMergedPr {
                    bookmark_name,
                    pr_number,
                });
                true
            }

            PrMergeStatus::Mergeable { bookmark_name, pr } => {
                println!(
                    "  Merging '{bookmark_name}' ({}, {})...",
                    fk.format_ref(pr.number), plan.options.merge_method
                );
                println!("    {}", pr.html_url);

                merge_with_retry(
                    github, owner, repo, pr.number, plan.options.merge_method, fk,
                )
                .with_context(|| {
                    format!("failed to merge {} for '{bookmark_name}'", fk.format_ref(pr.number))
                })?;

                merged.push(MergedPr {
                    bookmark_name,
                    pr_number: pr.number,
                    html_url: pr.html_url.clone(),
                });
                true
            }

            PrMergeStatus::Blocked {
                bookmark_name,
                pr,
                reasons,
            } => {
                let pr_label = pr
                    .as_ref()
                    .map(|p| format!(" ({})", fk.format_ref(p.number)))
                    .unwrap_or_default();
                println!("  Blocked at '{bookmark_name}'{pr_label}:");
                for reason in &reasons {
                    println!("    - {}", format_block_reason(reason, fk));
                }
                blocked_at = Some(BlockedPr {
                    bookmark_name,
                    pr_number: pr.as_ref().map(|p| p.number),
                    reasons,
                });
                break;
            }
        };

        // Reconcile after any resolved segment (merged or already-merged).
        if needs_reconcile && seg_idx + 1 < segments.len() {
            let fresh_map = reconcile_after_merge(
                jj, github, segments, seg_idx, plan, fk, &mut state,
            );
            pr_map = fresh_map;

            // Stop here if reconcile produced any failures. Continuing
            // risks merging the next PR with a bloated diff (local stack
            // never rebased) or against stale forge state.
            if let Some(blocked) = gate_after_reconcile(
                &state, &segments[seg_idx + 1], pr_map.as_ref(), fk,
            ) {
                blocked_at = Some(blocked);
                break;
            }
        }
    }

    Ok(MergeResult {
        merged,
        blocked_at,
        skipped_merged,
        local_warnings: state.warnings,
    })
}

/// Attempt to merge a PR with retry logic for transient HTTP errors.
///
/// Handles:
/// - 502/503: transient server errors — verify state, then retry
/// - 405 "already in progress": GitHub is processing — poll until merged
/// - Other errors: propagate immediately
pub(crate) fn merge_with_retry(
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
    number: u64,
    method: MergeMethod,
    fk: ForgeKind,
) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 3;

    for attempt in 0..MAX_ATTEMPTS {
        match forge.merge_pr(owner, repo, number, method) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if let Some(http_err) = e.downcast_ref::<HttpError>() {
                    match http_err.status {
                        502 | 503 => {
                            let wait = Duration::from_secs(2 * (attempt as u64 + 1));
                            println!(
                                "    Merge returned HTTP {}, verifying state...",
                                http_err.status
                            );
                            thread::sleep(wait);
                            if let Ok(state) = forge.get_pr_state(owner, repo, number)
                                && state.merged
                            {
                                println!("    {} was merged despite the error.", fk.format_ref(number));
                                return Ok(());
                            }
                            if attempt + 1 < MAX_ATTEMPTS {
                                println!("    Retrying...");
                            }
                            continue;
                        }
                        405 if http_err.body.contains("already in progress") => {
                            println!("    Merge already in progress, waiting...");
                            for _ in 0..10 {
                                thread::sleep(Duration::from_secs(3));
                                if let Ok(state) = forge.get_pr_state(owner, repo, number)
                                    && state.merged
                                {
                                    println!("    {} merged successfully.", fk.format_ref(number));
                                    return Ok(());
                                }
                            }
                            anyhow::bail!(
                                "merge of {} still in progress after 30s; check the forge manually",
                                fk.format_ref(number)
                            );
                        }
                        _ => return Err(e),
                    }
                }
                return Err(e);
            }
        }
    }
    anyhow::bail!(
        "merge of {} failed after {MAX_ATTEMPTS} attempts",
        fk.format_ref(number)
    );
}

fn execute_dry_run(plan: &MergePlan) -> Result<MergeResult> {
    let fk = plan.forge_kind;
    let mut merged = Vec::new();
    let mut blocked_at = None;
    let mut skipped_merged = Vec::new();

    for action in &plan.actions {
        match action {
            PrMergeStatus::AlreadyMerged {
                bookmark_name,
                pr_number,
            } => {
                println!(
                    "  Skipping '{bookmark_name}': {} already merged",
                    fk.format_ref(*pr_number)
                );
                skipped_merged.push(SkippedMergedPr {
                    bookmark_name: bookmark_name.clone(),
                    pr_number: *pr_number,
                });
            }
            PrMergeStatus::Mergeable { bookmark_name, pr } => {
                println!(
                    "  Would merge '{bookmark_name}' ({}, {})",
                    fk.format_ref(pr.number), plan.options.merge_method
                );
                merged.push(MergedPr {
                    bookmark_name: bookmark_name.clone(),
                    pr_number: pr.number,
                    html_url: pr.html_url.clone(),
                });
            }
            PrMergeStatus::Blocked {
                bookmark_name,
                pr,
                reasons,
            } => {
                let pr_label = pr
                    .as_ref()
                    .map(|p| format!(" ({})", fk.format_ref(p.number)))
                    .unwrap_or_default();
                println!("  Blocked at '{bookmark_name}'{pr_label}:");
                for reason in reasons {
                    println!("    - {}", format_block_reason(reason, fk));
                }
                blocked_at = Some(BlockedPr {
                    bookmark_name: bookmark_name.clone(),
                    pr_number: pr.as_ref().map(|p| p.number),
                    reasons: reasons.clone(),
                });
                break;
            }
        }
    }

    Ok(MergeResult {
        merged,
        blocked_at,
        skipped_merged,
        local_warnings: vec![],
    })
}

pub(crate) fn format_block_reason(reason: &BlockReason, fk: ForgeKind) -> String {
    let abbr = fk.request_abbreviation();
    match reason {
        BlockReason::NoPr => format!("No {abbr} exists for this bookmark"),
        BlockReason::Draft => format!("{abbr} is still a draft"),
        BlockReason::ChecksFailing => "CI checks are failing".to_string(),
        BlockReason::ChecksPending => "CI checks are pending".to_string(),
        BlockReason::InsufficientApprovals { have, need } => {
            format!("Insufficient approvals ({have}/{need})")
        }
        BlockReason::ChangesRequested => "Changes have been requested".to_string(),
        BlockReason::Conflicted => "Has merge conflicts".to_string(),
        BlockReason::MergeabilityUnknown => {
            "Mergeability is still being computed (try again in a moment)".to_string()
        }
        BlockReason::LocalSyncFailed => "Local sync failed".to_string(),
        BlockReason::ForgeReconcileFailed => "Forge reconcile failed".to_string(),
        BlockReason::ConcurrentModification => {
            "Concurrent modification (another jj process)".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use std::collections::HashMap;

    use super::*;
    use crate::forge::ForgeKind;
    use crate::forge::types::{
        ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequest,
        PullRequestRef, RepoInfo, ReviewSummary,
    };
    use crate::jj::types::{Bookmark, GitRemote, LogEntry};
    use crate::merge::plan::MergeOptions;

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
        }
    }

    fn repo_info() -> RepoInfo {
        RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        }
    }

    /// Test GitHub stub that records calls AND supports post-merge re-evaluation.
    /// merge_pr removes the PR from open_prs so subsequent list_open_prs reflects it.
    struct RecordingGitHub {
        calls: Mutex<Vec<String>>,
        open_prs: Mutex<Vec<PullRequest>>,
        merged_prs: HashMap<String, PullRequest>,
        mergeability: HashMap<u64, PrMergeability>,
        checks: HashMap<String, ChecksStatus>,
        reviews: HashMap<u64, ReviewSummary>,
    }

    impl RecordingGitHub {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                open_prs: Mutex::new(Vec::new()),
                merged_prs: HashMap::new(),
                mergeability: HashMap::new(),
                checks: HashMap::new(),
                reviews: HashMap::new(),
            }
        }

        fn with_evaluatable_pr(mut self, name: &str, number: u64) -> Self {
            self.open_prs
                .lock()
                .expect("poisoned")
                .push(make_pr(name, number));
            self.mergeability.insert(
                number,
                PrMergeability {
                    mergeable: Some(true),
                    mergeable_state: "clean".to_string(),
                },
            );
            self.checks
                .insert(format!("sha_{name}"), ChecksStatus::Pass);
            self.reviews.insert(
                number,
                ReviewSummary {
                    approved_count: 1,
                    changes_requested: false,
                },
            );
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("poisoned").clone()
        }
    }

    impl Forge for RecordingGitHub {
        fn merge_pr(&self, _o: &str, _r: &str, n: u64, m: MergeMethod) -> Result<()> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("merge_pr:#{n}:{m}"));
            self.open_prs
                .lock()
                .expect("poisoned")
                .retain(|pr| pr.number != n);
            Ok(())
        }
        fn update_pr_base(&self, _o: &str, _r: &str, n: u64, base: &str) -> Result<()> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("update_base:#{n}:{base}"));
            Ok(())
        }
        fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
            Ok(self.open_prs.lock().expect("poisoned").clone())
        }
        fn find_merged_pr(
            &self,
            _o: &str,
            _r: &str,
            head: &str,
        ) -> Result<Option<PullRequest>> {
            Ok(self.merged_prs.get(head).cloned())
        }
        fn get_pr_mergeability(
            &self,
            _o: &str,
            _r: &str,
            n: u64,
        ) -> Result<PrMergeability> {
            self.mergeability
                .get(&n)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no mergeability stub for PR #{n}"))
        }
        fn get_pr_checks_status(
            &self,
            _o: &str,
            _r: &str,
            head: &str,
        ) -> Result<ChecksStatus> {
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
        fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
        fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
        fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
        fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
        fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
        fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
        fn get_pr_state(&self, _o: &str, _r: &str, n: u64) -> Result<PrState> {
            self.calls.lock().expect("poisoned").push(format!("get_pr_state:#{n}"));
            Ok(PrState { merged: false, state: "open".to_string() })
        }
    }

    struct RecordingJj {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingJj {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("poisoned").clone()
        }
    }

    impl Jj for RecordingJj {
        fn git_fetch(&self) -> Result<()> {
            self.calls.lock().expect("poisoned").push("git_fetch".to_string());
            Ok(())
        }
        fn push_bookmark(&self, name: &str, remote: &str) -> Result<()> {
            self.calls.lock().expect("poisoned").push(format!("push:{name}:{remote}"));
            Ok(())
        }
        fn rebase_onto(&self, source: &str, dest: &str) -> Result<()> {
            self.calls.lock().expect("poisoned").push(format!("rebase:{source}:{dest}"));
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
        fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
        fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
        fn resolve_change_id(&self, change_id: &str) -> Result<Vec<String>> {
            self.calls.lock().expect("poisoned").push(format!("resolve_change_id:{change_id}"));
            Ok(vec!["dummy_commit_id".to_string()])
        }
        fn merge_into(&self, bookmark: &str, dest: &str) -> Result<()> {
            self.calls.lock().expect("poisoned").push(format!("merge_into:{bookmark}:{dest}"));
            Ok(())
        }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
    }

    /// Jj stub where push_bookmark always fails (simulates conflicted commits).
    struct FailingPushJj {
        calls: Mutex<Vec<String>>,
    }
    impl FailingPushJj {
        fn new() -> Self {
            Self { calls: Mutex::new(Vec::new()) }
        }
    }
    impl Jj for FailingPushJj {
        fn git_fetch(&self) -> Result<()> {
            self.calls.lock().expect("poisoned").push("git_fetch".to_string());
            Ok(())
        }
        fn push_bookmark(&self, name: &str, _remote: &str) -> Result<()> {
            self.calls.lock().expect("poisoned").push(format!("push:{name}"));
            anyhow::bail!("jj git push failed: conflicted commits")
        }
        fn rebase_onto(&self, source: &str, dest: &str) -> Result<()> {
            self.calls.lock().expect("poisoned").push(format!("rebase:{source}:{dest}"));
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
        fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
        fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
        fn resolve_change_id(&self, change_id: &str) -> Result<Vec<String>> {
            self.calls.lock().expect("poisoned").push(format!("resolve:{change_id}"));
            Ok(vec!["dummy".to_string()])
        }
        fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { Ok(()) }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
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

    fn make_plan_single_mergeable(name: &str, pr_number: u64) -> MergePlan {
        MergePlan {
            actions: vec![PrMergeStatus::Mergeable {
                bookmark_name: name.to_string(),
                pr: make_pr(name, pr_number),
            }],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        }
    }

    #[test]
    fn test_dry_run_no_api_calls() {
        let jj = RecordingJj::new();
        let gh = RecordingGitHub::new();
        let plan = make_plan_single_mergeable("auth", 1);
        let segments = vec![make_segment("auth")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, true).unwrap();

        assert_eq!(result.merged.len(), 1);
        assert!(jj.calls().is_empty());
        assert!(gh.calls().is_empty());
    }

    #[test]
    fn test_single_merge() {
        let jj = RecordingJj::new();
        let gh = RecordingGitHub::new().with_evaluatable_pr("auth", 1);
        let plan = make_plan_single_mergeable("auth", 1);
        let segments = vec![make_segment("auth")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.merged.len(), 1);
        assert_eq!(result.merged[0].pr_number, 1);
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#1:squash"));
        // No remaining segments → no fetch/rebase/push
        assert!(jj.calls().is_empty());
    }

    #[test]
    fn test_merge_with_remaining_stack() {
        let jj = RecordingJj::new();
        // After merging auth, profile will be re-evaluated against fresh GitHub state.
        // Set up profile as open with pending CI so it blocks.
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        // Profile's base points at auth (needs retargeting)
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.merged.len(), 1);
        assert!(result.blocked_at.is_some());

        let jj_calls = jj.calls();
        assert!(jj_calls.contains(&"git_fetch".to_string()));
        assert!(jj_calls.iter().any(|c| c.starts_with("rebase:ch_profile:main")));
        assert!(jj_calls.iter().any(|c| c == "push:profile:origin"));

        // Should retarget profile PR from auth → main
        assert!(gh.calls().iter().any(|c| c == "update_base:#2:main"));

        // Happy path: no local warnings
        assert!(result.local_warnings.is_empty(), "happy path should have no local warnings");
    }

    #[test]
    fn test_config_default_reconciles_with_rebase() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();

        let config = crate::config::Config::default();
        let opts = MergeOptions {
            merge_method: config.merge_method,
            required_approvals: config.required_approvals,
            require_ci_pass: config.require_ci_pass,
            reconcile_strategy: config.reconcile_strategy,
            ready: false,
        };

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: opts,
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();
        assert_eq!(result.merged.len(), 1);

        let jj_calls = jj.calls();
        assert!(
            jj_calls.iter().any(|c| c.starts_with("rebase:")),
            "Config::default() should reconcile with rebase, got: {jj_calls:?}"
        );
        assert!(
            !jj_calls.iter().any(|c| c.starts_with("merge_into:")),
            "Config::default() should not use merge_into, got: {jj_calls:?}"
        );
    }

    #[test]
    fn test_rebase_uses_oldest_commit_in_segment() {
        // When a segment has multiple commits (e.g., 3 commits between two bookmarks),
        // the rebase must start from the oldest commit (closest to the merged bookmark),
        // not the bookmark tip. Otherwise intermediate commits are orphaned.
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        // Profile segment has 3 commits: tip (bookmark) + 2 intermediate.
        // changes is newest-first, so: [tip, middle, oldest]
        let profile_segment = NarrowedSegment {
            bookmark: Bookmark {
                name: "profile".to_string(),
                commit_id: "c_profile".to_string(),
                change_id: "ch_profile".to_string(),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![
                LogEntry {
                    commit_id: "c_profile".to_string(),
                    change_id: "ch_profile".to_string(),
                    author_name: "Test".to_string(),
                    author_email: "test@test.com".to_string(),
                    description: "Add profile UI".to_string(),
                    description_first_line: "Add profile UI".to_string(),
                    parents: vec!["c_middle".to_string()],
                    local_bookmarks: vec!["profile".to_string()],
                    remote_bookmarks: vec![],
                    is_working_copy: false,
                    conflict: false,
                    empty: false,
                },
                LogEntry {
                    commit_id: "c_middle".to_string(),
                    change_id: "ch_middle".to_string(),
                    author_name: "Test".to_string(),
                    author_email: "test@test.com".to_string(),
                    description: "Add profile helpers".to_string(),
                    description_first_line: "Add profile helpers".to_string(),
                    parents: vec!["c_oldest".to_string()],
                    local_bookmarks: vec![],
                    remote_bookmarks: vec![],
                    is_working_copy: false,
                    conflict: false,
                    empty: false,
                },
                LogEntry {
                    commit_id: "c_oldest".to_string(),
                    change_id: "ch_oldest".to_string(),
                    author_name: "Test".to_string(),
                    author_email: "test@test.com".to_string(),
                    description: "Add profile model".to_string(),
                    description_first_line: "Add profile model".to_string(),
                    parents: vec!["c_auth".to_string()],
                    local_bookmarks: vec![],
                    remote_bookmarks: vec![],
                    is_working_copy: false,
                    conflict: false,
                    empty: false,
                },
            ],
            merge_source_names: vec![],
        };
        let segments = vec![make_segment("auth"), profile_segment];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();
        assert_eq!(result.merged.len(), 1);

        // Must rebase from ch_oldest (the first commit after auth), NOT ch_profile (the tip).
        // Rebasing from the tip would orphan c_middle and c_oldest.
        let jj_calls = jj.calls();
        assert!(
            jj_calls.iter().any(|c| c == "rebase:ch_oldest:main"),
            "should rebase from oldest commit in segment, got: {jj_calls:?}"
        );
        assert!(
            !jj_calls.iter().any(|c| c == "rebase:ch_profile:main"),
            "should NOT rebase from bookmark tip: {jj_calls:?}"
        );
    }

    #[test]
    fn test_merge_strategy_calls_merge_into() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();

        let mut opts = default_options();
        opts.reconcile_strategy = crate::config::ReconcileStrategy::Merge;

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: opts,
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();
        assert_eq!(result.merged.len(), 1);

        let jj_calls = jj.calls();
        // Should call merge_into instead of rebase_onto
        assert!(
            jj_calls.iter().any(|c| c == "merge_into:profile:main"),
            "merge strategy should call merge_into, got: {jj_calls:?}"
        );
        assert!(
            !jj_calls.iter().any(|c| c.starts_with("rebase:")),
            "merge strategy should NOT call rebase_onto: {jj_calls:?}"
        );
        // Should still push
        assert!(jj_calls.iter().any(|c| c == "push:profile:origin"));
    }

    #[test]
    fn test_rebase_strategy_does_not_call_merge_into() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(), // Rebase is default in tests
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();
        assert_eq!(result.merged.len(), 1);

        let jj_calls = jj.calls();
        assert!(
            jj_calls.iter().any(|c| c.starts_with("rebase:")),
            "rebase strategy should call rebase_onto: {jj_calls:?}"
        );
        assert!(
            !jj_calls.iter().any(|c| c.starts_with("merge_into:")),
            "rebase strategy should NOT call merge_into: {jj_calls:?}"
        );
    }

    #[test]
    fn test_merge_strategy_syncs_all_remaining_segments() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2)
            .with_evaluatable_pr("settings", 3);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        gh.checks.insert("sha_settings".to_string(), ChecksStatus::Pending);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();
        gh.open_prs.lock().expect("poisoned")[2]
            .base
            .ref_name = "profile".to_string();

        let mut opts = default_options();
        opts.reconcile_strategy = crate::config::ReconcileStrategy::Merge;

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "settings".to_string(),
                    pr: Some(make_pr("settings", 3)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: opts,
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();
        assert_eq!(result.merged.len(), 1);

        let jj_calls = jj.calls();
        // Both remaining bookmarks should get merge_into
        assert!(
            jj_calls.iter().any(|c| c == "merge_into:profile:main"),
            "should merge_into profile: {jj_calls:?}"
        );
        assert!(
            jj_calls.iter().any(|c| c == "merge_into:settings:main"),
            "should merge_into settings: {jj_calls:?}"
        );
        // Both should be pushed
        assert!(jj_calls.iter().any(|c| c == "push:profile:origin"));
        assert!(jj_calls.iter().any(|c| c == "push:settings:origin"));
    }

    #[test]
    fn test_merge_failure_skips_push_for_failed_bookmark() {
        struct FailingMergeJj {
            calls: Mutex<Vec<String>>,
        }
        impl FailingMergeJj {
            fn new() -> Self { Self { calls: Mutex::new(Vec::new()) } }
            fn calls(&self) -> Vec<String> { self.calls.lock().expect("poisoned").clone() }
        }
        impl Jj for FailingMergeJj {
            fn git_fetch(&self) -> Result<()> {
                self.calls.lock().expect("poisoned").push("git_fetch".to_string());
                Ok(())
            }
            fn push_bookmark(&self, name: &str, remote: &str) -> Result<()> {
                self.calls.lock().expect("poisoned").push(format!("push:{name}:{remote}"));
                Ok(())
            }
            fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { Ok(()) }
            fn merge_into(&self, bookmark: &str, _dest: &str) -> Result<()> {
                self.calls.lock().expect("poisoned").push(format!("merge_into:{bookmark}"));
                if bookmark == "profile" {
                    anyhow::bail!("merge conflict in profile")
                }
                Ok(())
            }
            fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
            fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
                Ok(vec!["dummy".to_string()])
            }
            fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
        }

        let jj = FailingMergeJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2)
            .with_evaluatable_pr("settings", 3);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        gh.checks.insert("sha_settings".to_string(), ChecksStatus::Pending);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();
        gh.open_prs.lock().expect("poisoned")[2]
            .base
            .ref_name = "profile".to_string();

        let mut opts = default_options();
        opts.reconcile_strategy = crate::config::ReconcileStrategy::Merge;

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "settings".to_string(),
                    pr: Some(make_pr("settings", 3)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: opts,
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        let jj_calls = jj.calls();
        // merge_into attempted for profile, but breaks on failure — settings not attempted
        assert!(jj_calls.iter().any(|c| c == "merge_into:profile"));
        assert!(
            !jj_calls.iter().any(|c| c == "merge_into:settings"),
            "should stop after first merge_into failure: {jj_calls:?}"
        );
        // Neither should be pushed
        assert!(
            !jj_calls.iter().any(|c| c == "push:profile:origin"),
            "should NOT push bookmark whose merge_into failed: {jj_calls:?}"
        );
        assert!(
            !jj_calls.iter().any(|c| c == "push:settings:origin"),
            "should NOT push downstream bookmark after failure: {jj_calls:?}"
        );
        // Should have warnings about the failure
        assert!(!result.local_warnings.is_empty());
    }

    #[test]
    fn test_merge_conflict_detected_skips_push() {
        struct ConflictingMergeJj {
            calls: Mutex<Vec<String>>,
        }
        impl ConflictingMergeJj {
            fn new() -> Self { Self { calls: Mutex::new(Vec::new()) } }
            fn calls(&self) -> Vec<String> { self.calls.lock().expect("poisoned").clone() }
        }
        impl Jj for ConflictingMergeJj {
            fn git_fetch(&self) -> Result<()> {
                self.calls.lock().expect("poisoned").push("git_fetch".to_string());
                Ok(())
            }
            fn push_bookmark(&self, name: &str, remote: &str) -> Result<()> {
                self.calls.lock().expect("poisoned").push(format!("push:{name}:{remote}"));
                Ok(())
            }
            fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { Ok(()) }
            fn merge_into(&self, bookmark: &str, dest: &str) -> Result<()> {
                self.calls.lock().expect("poisoned").push(format!("merge_into:{bookmark}:{dest}"));
                Ok(())
            }
            fn is_conflicted(&self, revset: &str) -> Result<bool> {
                // First bookmark in remaining stack has conflicts
                Ok(revset == "profile")
            }
            fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
            fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
                Ok(vec!["dummy".to_string()])
            }
        }

        let jj = ConflictingMergeJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2)
            .with_evaluatable_pr("settings", 3);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        gh.checks.insert("sha_settings".to_string(), ChecksStatus::Pending);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();
        gh.open_prs.lock().expect("poisoned")[2]
            .base
            .ref_name = "profile".to_string();

        let mut opts = default_options();
        opts.reconcile_strategy = crate::config::ReconcileStrategy::Merge;

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "settings".to_string(),
                    pr: Some(make_pr("settings", 3)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: opts,
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        let jj_calls = jj.calls();
        // merge_into succeeds but conflict detected — should not push or continue
        assert!(jj_calls.iter().any(|c| c == "merge_into:profile:main"));
        assert!(
            !jj_calls.iter().any(|c| c == "merge_into:settings:main"),
            "should stop after first conflicted bookmark: {jj_calls:?}"
        );
        assert!(
            !jj_calls.iter().any(|c| c.starts_with("push:")),
            "should not push any bookmark when conflict detected: {jj_calls:?}"
        );
        // Warning should mention the conflict
        assert!(
            result.local_warnings.iter().any(|w| w.message.contains("has conflicts")),
            "should warn about conflicts: {:?}", result.local_warnings
        );
    }

    #[test]
    fn test_no_retarget_when_base_already_correct() {
        let jj = RecordingJj::new();
        // Profile PR's base is already "main" (the default from make_pr)
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        // Should NOT call update_base since it's already "main"
        assert!(
            !gh.calls().iter().any(|c| c.starts_with("update_base")),
            "should not retarget when base is already correct: {:?}",
            gh.calls()
        );
        assert!(result.local_warnings.is_empty(), "happy path should have no local warnings");
    }

    #[test]
    fn test_push_uses_plan_remote_name() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "upstream".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert!(
            jj.calls().iter().any(|c| c == "push:profile:upstream"),
            "should push to the remote from the plan, not hardcoded origin: {:?}",
            jj.calls()
        );
        assert!(result.local_warnings.is_empty(), "happy path should have no local warnings");
    }

    #[test]
    fn test_already_merged_skipped() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("profile", 2);
        gh.merged_prs.insert("auth".to_string(), make_pr("auth", 1));

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::AlreadyMerged {
                    bookmark_name: "auth".to_string(),
                    pr_number: 1,
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.skipped_merged.len(), 1);
        assert_eq!(result.skipped_merged[0].pr_number, 1);
        assert_eq!(result.merged.len(), 1);
        assert_eq!(result.merged[0].pr_number, 2);
    }

    #[test]
    fn test_reconciles_after_already_merged() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("profile", 2);
        gh.merged_prs.insert("auth".to_string(), make_pr("auth", 1));

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::AlreadyMerged {
                    bookmark_name: "auth".to_string(),
                    pr_number: 1,
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.skipped_merged.len(), 1);
        assert_eq!(result.merged.len(), 1);

        let jj_calls = jj.calls();
        assert!(
            jj_calls.iter().any(|c| c == "git_fetch"),
            "reconcile should run after AlreadyMerged when more segments remain: {jj_calls:?}"
        );
    }

    #[test]
    fn test_blocked_stops_execution() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("auth", 1);
        // Make auth a draft with failing CI so it blocks
        gh.open_prs.lock().expect("poisoned")[0].draft = true;
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::Fail);

        let plan = MergePlan {
            actions: vec![PrMergeStatus::Blocked {
                bookmark_name: "auth".to_string(),
                pr: Some(make_pr("auth", 1)),
                reasons: vec![BlockReason::Draft, BlockReason::ChecksFailing],
            }],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert!(result.merged.is_empty());
        assert!(result.blocked_at.is_some());
        let blocked = result.blocked_at.unwrap();
        assert_eq!(blocked.bookmark_name, "auth");
        assert_eq!(blocked.reasons.len(), 2);
        assert!(gh.calls().is_empty());
    }

    #[test]
    fn test_merge_failure_reports_error() {
        struct FailingMergeGitHub;
        impl Forge for FailingMergeGitHub {
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> {
                anyhow::bail!("merge conflict detected")
            }
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![make_pr("auth", 1)])
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { Ok(None) }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { Ok(ChecksStatus::Pass) }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> {
                Ok(ReviewSummary { approved_count: 1, changes_requested: false })
            }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> {
                Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".to_string() })
            }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let jj = RecordingJj::new();
        let plan = make_plan_single_mergeable("auth", 1);
        let segments = vec![make_segment("auth")];

        let err = execute_merge_plan(&jj, &FailingMergeGitHub, &plan, &segments, false).unwrap_err();
        assert!(format!("{err:#}").contains("merge conflict detected"));
    }

    #[test]
    fn test_multi_merge_chain() {
        let jj = RecordingJj::new();
        // Both PRs need eval data so re-evaluation after merging auth finds profile mergeable
        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.merged.len(), 2);
        let gh_calls = gh.calls();
        assert!(gh_calls.iter().any(|c| c == "merge_pr:#1:squash"));
        assert!(gh_calls.iter().any(|c| c == "merge_pr:#2:squash"));
    }

    #[test]
    fn test_three_segment_merge_chain() {
        let jj = RecordingJj::new();
        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2)
            .with_evaluatable_pr("settings", 3);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "settings".to_string(),
                    pr: make_pr("settings", 3),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.merged.len(), 3);
        assert!(result.blocked_at.is_none());

        let gh_calls = gh.calls();
        assert_eq!(
            gh_calls.iter().filter(|c| c.starts_with("merge_pr")).count(),
            3,
            "should merge all 3 PRs: {gh_calls:?}"
        );

        let jj_calls = jj.calls();
        assert_eq!(
            jj_calls.iter().filter(|c| c == &"git_fetch").count(),
            2,
            "should fetch after first two merges: {jj_calls:?}"
        );
        assert_eq!(
            jj_calls.iter().filter(|c| c.starts_with("rebase:")).count(),
            2,
            "should rebase after first two merges: {jj_calls:?}"
        );
        assert_eq!(
            jj_calls.iter().filter(|c| c.starts_with("push:")).count(),
            3,
            "should push 2+1 remaining bookmarks: {jj_calls:?}"
        );
        assert!(jj_calls.iter().any(|c| c == "push:settings:origin"));
    }

    #[test]
    fn test_recheck_after_merge_discovers_concurrent_merge() {
        // auth and profile are Mergeable in the plan. We merge auth.
        // While we were merging, someone else merged profile externally.
        // Re-evaluation should discover profile as AlreadyMerged and skip it.
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("auth", 1);
        // Profile is NOT in open_prs (someone already merged it)
        // but IS in merged_prs so find_merged_pr finds it
        gh.merged_prs.insert(
            "profile".to_string(),
            PullRequest {
                merged_at: Some("2024-01-01T00:00:00Z".to_string()),
                ..make_pr("profile", 2)
            },
        );

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.merged.len(), 1);
        assert_eq!(result.merged[0].bookmark_name, "auth");

        assert_eq!(result.skipped_merged.len(), 1);
        assert_eq!(result.skipped_merged[0].bookmark_name, "profile");
        assert_eq!(result.skipped_merged[0].pr_number, 2);

        // Should NOT have called merge_pr for profile
        assert!(
            !gh.calls().iter().any(|c| c.contains("#2")),
            "should not merge profile when it was already merged: {:?}",
            gh.calls()
        );
    }

    #[test]
    fn test_recheck_after_merge_detects_pending_ci() {
        // The upfront plan says both are Mergeable, but after merging auth,
        // re-evaluation against live state finds profile now has pending CI.
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        // Override: profile CI is now pending (simulating CI re-running on rebased code)
        gh.checks
            .insert("sha_profile".to_string(), ChecksStatus::Pending);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                // Plan says Mergeable, but live re-evaluation should catch pending CI
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        // Auth should be merged
        assert_eq!(result.merged.len(), 1);
        assert_eq!(result.merged[0].bookmark_name, "auth");

        // Profile should be blocked due to pending CI (from live re-evaluation)
        let blocked = result.blocked_at.as_ref().expect("should be blocked");
        assert_eq!(blocked.bookmark_name, "profile");
        assert!(
            blocked.reasons.contains(&BlockReason::ChecksPending),
            "re-evaluation should detect pending CI: {:?}",
            blocked.reasons
        );

        // Should NOT have called merge_pr for profile
        assert!(
            !gh.calls().iter().any(|c| c.contains("#2")),
            "should not merge profile when CI is pending: {:?}",
            gh.calls()
        );
    }

    #[test]
    fn test_merge_with_stack_base_retargets_to_base() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("sha_profile".to_string(), ChecksStatus::Pending);
        // Profile's base still points at auth (needs retarget to coworker-feat, not main)
        gh.open_prs.lock().expect("poisoned")[0]
            .base
            .ref_name = "auth".to_string();

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Blocked {
                    bookmark_name: "profile".to_string(),
                    pr: Some(make_pr("profile", 2)),
                    reasons: vec![BlockReason::ChecksPending],
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: Some("coworker-feat".to_string()),
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        // Should rebase onto coworker-feat, not main
        assert!(
            jj.calls().iter().any(|c| c == "rebase:ch_profile:coworker-feat"),
            "should rebase onto stack_base: {:?}",
            jj.calls()
        );
        // Should retarget to coworker-feat, not main
        assert!(
            gh.calls().iter().any(|c| c == "update_base:#2:coworker-feat"),
            "should retarget to stack_base: {:?}",
            gh.calls()
        );
    }

    #[test]
    fn test_format_block_reasons_github() {
        let fk = ForgeKind::GitHub;
        assert_eq!(format_block_reason(&BlockReason::NoPr, fk), "No PR exists for this bookmark");
        assert_eq!(format_block_reason(&BlockReason::Draft, fk), "PR is still a draft");
        assert_eq!(format_block_reason(&BlockReason::ChecksFailing, fk), "CI checks are failing");
        assert_eq!(format_block_reason(&BlockReason::ChecksPending, fk), "CI checks are pending");
        assert_eq!(
            format_block_reason(&BlockReason::InsufficientApprovals { have: 0, need: 2 }, fk),
            "Insufficient approvals (0/2)"
        );
        assert_eq!(format_block_reason(&BlockReason::ChangesRequested, fk), "Changes have been requested");
        assert_eq!(format_block_reason(&BlockReason::Conflicted, fk), "Has merge conflicts");
        assert!(format_block_reason(&BlockReason::MergeabilityUnknown, fk).contains("still being computed"));
    }

    #[test]
    fn test_format_block_reasons_gitlab() {
        let fk = ForgeKind::GitLab;
        assert_eq!(format_block_reason(&BlockReason::NoPr, fk), "No MR exists for this bookmark");
        assert_eq!(format_block_reason(&BlockReason::Draft, fk), "MR is still a draft");
    }

    #[test]
    fn test_merge_retry_on_502_then_verified_merged() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct RetryGitHub {
            attempt: AtomicU32,
        }
        impl Forge for RetryGitHub {
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> {
                let n = self.attempt.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(crate::forge::http::HttpError {
                        status: 502,
                        method: "PUT".to_string(),
                        path: "repos/o/r/pulls/1/merge".to_string(),
                        body: "Bad Gateway".to_string(),
                    }.into())
                } else {
                    Ok(())
                }
            }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> { Ok(vec![]) }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { Ok(None) }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
        }

        let result = merge_with_retry(
            &RetryGitHub { attempt: AtomicU32::new(0) },
            "o", "r", 1, MergeMethod::Squash, ForgeKind::GitHub,
        );
        assert!(result.is_ok(), "should succeed after retry: {result:?}");
    }

    #[test]
    fn test_merge_retry_on_405_already_in_progress_verified_merged() {
        struct AlreadyInProgressGitHub;
        impl Forge for AlreadyInProgressGitHub {
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> {
                Err(crate::forge::http::HttpError {
                    status: 405,
                    method: "PUT".to_string(),
                    path: "repos/o/r/pulls/1/merge".to_string(),
                    body: r#"{"message":"Merge already in progress"}"#.to_string(),
                }.into())
            }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: true, state: "closed".to_string() })
            }
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> { Ok(vec![]) }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { Ok(None) }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
        }

        let result = merge_with_retry(
            &AlreadyInProgressGitHub,
            "o", "r", 1, MergeMethod::Squash, ForgeKind::GitHub,
        );
        assert!(result.is_ok(), "should succeed when state shows merged: {result:?}");
    }

    #[test]
    fn test_merge_no_retry_on_400() {
        struct BadRequestGitHub;
        impl Forge for BadRequestGitHub {
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> {
                Err(crate::forge::http::HttpError {
                    status: 400,
                    method: "PUT".to_string(),
                    path: "repos/o/r/pulls/1/merge".to_string(),
                    body: "Bad request".to_string(),
                }.into())
            }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> { Ok(vec![]) }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { Ok(None) }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
        }

        let result = merge_with_retry(
            &BadRequestGitHub,
            "o", "r", 1, MergeMethod::Squash, ForgeKind::GitHub,
        );
        assert!(result.is_err(), "should fail immediately on 400");
    }

    #[test]
    fn test_divergent_change_id_blocks_subsequent_merges() {
        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);

        // RecordingJj that returns 2 commit IDs for resolve_change_id
        struct DivergentJj;
        impl Jj for DivergentJj {
            fn git_fetch(&self) -> Result<()> { Ok(()) }
            fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> { Ok(()) }
            fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { Ok(()) }
            fn get_my_bookmarks(&self) -> Result<Vec<crate::jj::types::Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<crate::jj::types::LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<crate::jj::types::GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
            fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
                Ok(vec!["commit_a".to_string(), "commit_b".to_string()])
            }
            fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { Ok(()) }
            fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
        }

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&DivergentJj, &gh, &plan, &segments, false).unwrap();

        // auth merges; profile is gated because reconcile detected divergence.
        // Continuing would risk merging profile with a bloated diff.
        assert_eq!(result.merged.len(), 1, "only auth should merge: {:?}", result.merged);
        assert_eq!(result.merged[0].bookmark_name, "auth");
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#1:squash"));
        assert!(
            !gh.calls().iter().any(|c| c == "merge_pr:#2:squash"),
            "profile must NOT merge while local is divergent"
        );
        let blocked = result.blocked_at.expect("profile should be blocked");
        assert_eq!(blocked.bookmark_name, "profile");
        assert!(blocked.reasons.contains(&BlockReason::LocalSyncFailed));
        assert!(
            result.local_warnings.iter().any(|w| w.message.contains("divergent")),
            "divergence should still be reported: {:?}", result.local_warnings
        );
    }

    #[test]
    fn test_block_reason_is_transient() {
        assert!(BlockReason::ChecksPending.is_transient());
        assert!(BlockReason::MergeabilityUnknown.is_transient());
        assert!(!BlockReason::Draft.is_transient());
        assert!(!BlockReason::NoPr.is_transient());
        assert!(!BlockReason::ChecksFailing.is_transient());
        assert!(!BlockReason::ChangesRequested.is_transient());
        assert!(!BlockReason::Conflicted.is_transient());
        assert!(
            !BlockReason::InsufficientApprovals { have: 0, need: 1 }.is_transient()
        );
        // LocalSyncFailed and ForgeReconcileFailed need user action; not transient.
        assert!(!BlockReason::LocalSyncFailed.is_transient());
        assert!(!BlockReason::ForgeReconcileFailed.is_transient());
    }

    #[test]
    fn reconcile_state_default_is_clean() {
        let s = ReconcileState::default();
        assert!(!s.degraded());
        assert!(s.block_reasons().is_empty());
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn reconcile_state_local_failure_is_degraded() {
        let s = ReconcileState {
            local_failed: true,
            forge_failed: false,
            warnings: vec![LocalDivergenceWarning {
                kind: DivergenceKind::Local,
                message: "fetch failed".into(),
            }],
        };
        assert!(s.degraded());
        assert_eq!(s.block_reasons(), vec![BlockReason::LocalSyncFailed]);
    }

    #[test]
    fn reconcile_state_forge_failure_is_degraded() {
        let s = ReconcileState {
            local_failed: false,
            forge_failed: true,
            warnings: vec![LocalDivergenceWarning {
                kind: DivergenceKind::Forge,
                message: "list_open_prs failed".into(),
            }],
        };
        assert!(s.degraded());
        assert_eq!(s.block_reasons(), vec![BlockReason::ForgeReconcileFailed]);
    }

    #[test]
    fn reconcile_state_both_failures_emit_both_reasons() {
        let s = ReconcileState {
            local_failed: true,
            forge_failed: true,
            warnings: vec![],
        };
        assert!(s.degraded());
        let reasons = s.block_reasons();
        assert!(reasons.contains(&BlockReason::LocalSyncFailed));
        assert!(reasons.contains(&BlockReason::ForgeReconcileFailed));
        assert_eq!(reasons.len(), 2);
    }

    #[test]
    fn reconcile_state_concurrent_warning_degrades_and_reports() {
        // A Concurrent-kind warning (no local_failed/forge_failed flag) must
        // still degrade the state and report ConcurrentModification.
        let s = ReconcileState {
            local_failed: false,
            forge_failed: false,
            warnings: vec![LocalDivergenceWarning {
                kind: DivergenceKind::Concurrent,
                message: "rolled back".into(),
            }],
        };
        assert!(s.has_concurrent());
        assert!(s.degraded());
        assert_eq!(s.block_reasons(), vec![BlockReason::ConcurrentModification]);
        assert!(BlockReason::ConcurrentModification.is_transient());
    }

    /// Scriptable reconcile stub. `current_operation_id` returns the post-fetch op
    /// "pf" (the restore target); `divergent` is the standing divergent-change set
    /// and `divergent_after_rebase` the divergence a mangling rebase introduces.
    /// Records fetch/rebase/restore/push so tests can assert the exact actions.
    struct FakeReconcileJj {
        divergent: Vec<String>,
        divergent_after_rebase: Vec<String>,
        divergent_errors: bool,
        fetched: Mutex<bool>,
        rebased: Mutex<bool>,
        restored: Mutex<Vec<String>>,
        pushed: Mutex<Vec<String>>,
    }
    impl FakeReconcileJj {
        fn new() -> Self {
            Self {
                divergent: vec![],
                divergent_after_rebase: vec![],
                divergent_errors: false,
                fetched: Mutex::new(false),
                rebased: Mutex::new(false),
                restored: Mutex::new(vec![]),
                pushed: Mutex::new(vec![]),
            }
        }
        fn divergent(mut self, v: &[&str]) -> Self {
            self.divergent = v.iter().map(|s| s.to_string()).collect();
            self
        }
        /// The divergence read fails (models lock contention from a concurrent
        /// writer — `jj log -r divergent()` erroring transiently).
        fn divergent_errors(mut self) -> Self {
            self.divergent_errors = true;
            self
        }
        /// Divergence that only appears once the rebase has run — i.e. the rebase
        /// itself introduced it (the corruption signature).
        fn divergent_after_rebase(mut self, v: &[&str]) -> Self {
            self.divergent_after_rebase = v.iter().map(|s| s.to_string()).collect();
            self
        }
    }
    impl Jj for FakeReconcileJj {
        fn current_operation_id(&self) -> Result<String> {
            Ok("pf".to_string())
        }
        fn divergent_change_ids(&self) -> Result<Vec<String>> {
            if self.divergent_errors {
                anyhow::bail!("lock contention reading divergent()");
            }
            let mut ids = self.divergent.clone();
            if *self.rebased.lock().expect("poisoned") {
                ids.extend(self.divergent_after_rebase.iter().cloned());
            }
            Ok(ids)
        }
        fn git_fetch(&self) -> Result<()> {
            *self.fetched.lock().expect("poisoned") = true;
            Ok(())
        }
        fn rebase_onto(&self, _: &str, _: &str) -> Result<()> {
            *self.rebased.lock().expect("poisoned") = true;
            Ok(())
        }
        fn restore_operation(&self, op: &str) -> Result<()> {
            self.restored.lock().expect("poisoned").push(op.to_string());
            Ok(())
        }
        fn push_bookmark(&self, name: &str, _: &str) -> Result<()> {
            self.pushed.lock().expect("poisoned").push(name.to_string());
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
        fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".into()) }
        fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".into()) }
        fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> { Ok(vec!["c".into()]) }
        fn merge_into(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn is_conflicted(&self, _: &str) -> Result<bool> { Ok(false) }
    }

    fn reconcile_two(jj: &dyn Jj) -> Vec<LocalDivergenceWarning> {
        let segments = vec![make_segment("bottom"), make_segment("top")];
        reconcile_local_state(
            jj, &segments, 0, "main", "origin",
            crate::config::ReconcileStrategy::Rebase,
        )
    }

    #[test]
    fn reconcile_gates_when_already_divergent() {
        // Already divergent at start: don't rebase a divergent stack, don't roll
        // back — preserve both versions in place and surface the change id.
        let jj = FakeReconcileJj::new().divergent(&["ch_x"]);
        let warnings = reconcile_two(&jj);

        assert_eq!(warnings.len(), 1, "got {warnings:?}");
        assert_eq!(warnings[0].kind, DivergenceKind::Concurrent);
        assert!(warnings[0].message.contains("ch_x"), "should name the divergent change");
        assert!(!*jj.fetched.lock().unwrap(), "gates before touching the repo");
        assert!(!*jj.rebased.lock().unwrap());
        assert!(jj.restored.lock().unwrap().is_empty());
        assert!(jj.pushed.lock().unwrap().is_empty());
    }

    #[test]
    fn reconcile_restores_to_post_fetch_when_rebase_introduces_divergence() {
        // Fetch is clean; the rebase itself introduces divergence (the corruption
        // signature — it raced a concurrent reconcile). jjpr must undo ONLY the
        // rebase: restore to the post-fetch op ("pf"), never past it to "op0"
        // (which would discard the concurrent process's work), and never push.
        let jj = FakeReconcileJj::new().divergent_after_rebase(&["ch_b"]);
        let warnings = reconcile_two(&jj);

        assert_eq!(warnings.len(), 1, "got {warnings:?}");
        assert_eq!(warnings[0].kind, DivergenceKind::Concurrent);
        assert!(*jj.rebased.lock().unwrap(), "rebase was attempted (fetch was clean)");
        // Restore targets exactly the post-fetch op — the only op captured, so we
        // structurally cannot roll back past the fetch.
        assert_eq!(*jj.restored.lock().unwrap(), vec!["pf".to_string()]);
        assert!(jj.pushed.lock().unwrap().is_empty(), "must not push a mangled tree");
    }

    #[test]
    fn reconcile_proceeds_and_pushes_when_clean() {
        // No concurrency, no divergence: the normal fast path — rebase and push,
        // never rolling back.
        let jj = FakeReconcileJj::new();
        let warnings = reconcile_two(&jj);

        assert!(
            !warnings.iter().any(|w| w.kind == DivergenceKind::Concurrent),
            "got {warnings:?}"
        );
        assert!(*jj.rebased.lock().unwrap());
        assert_eq!(*jj.pushed.lock().unwrap(), vec!["top".to_string()]);
        assert!(jj.restored.lock().unwrap().is_empty());
    }

    #[test]
    fn reconcile_gates_when_the_divergence_read_errors() {
        // The divergence signal is what a concurrent writer's lock contention
        // makes fail — and it fails BEFORE we know the state is clean. A read
        // error must gate (never push), not be mistaken for "no divergence".
        let jj = FakeReconcileJj::new().divergent_errors();
        let warnings = reconcile_two(&jj);

        assert_eq!(warnings.len(), 1, "got {warnings:?}");
        assert_eq!(warnings[0].kind, DivergenceKind::Concurrent);
        assert!(!*jj.rebased.lock().unwrap(), "must not rebase when it can't verify the state");
        assert!(jj.pushed.lock().unwrap().is_empty(), "must not push when it can't verify the state");
    }

    /// `block_reasons()` ordering is load-bearing: prev_reconcile_block
    /// in run_watch_loop compares Vec<BlockReason> via PartialEq, which
    /// is order-sensitive. If this swaps order across calls or releases,
    /// the recovery-message logic flips between "different" reads on
    /// every iteration, causing reprint storms.
    /// A: rebase_root must point at the OLDEST commit in a multi-commit
    /// segment. Otherwise `jj rebase -s <root>` strands earlier commits
    /// when the user follows recovery hints.
    #[test]
    fn rebase_root_uses_oldest_commit_for_multi_commit_segment() {
        // jj log emits newest-first, so changes[0] is the tip and
        // changes.last() is the oldest. Build a segment with two
        // distinct change ids and the bookmark on the tip.
        let seg = NarrowedSegment {
            bookmark: Bookmark {
                name: "feature".to_string(),
                commit_id: "c_tip".to_string(),
                change_id: "ch_tip".to_string(),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![
                LogEntry {
                    commit_id: "c_tip".to_string(),
                    change_id: "ch_tip".to_string(),
                    author_name: "T".to_string(),
                    author_email: "t@x".to_string(),
                    description: "tip".to_string(),
                    description_first_line: "tip".to_string(),
                    parents: vec!["c_root".to_string()],
                    local_bookmarks: vec!["feature".to_string()],
                    remote_bookmarks: vec![],
                    is_working_copy: false,
                    conflict: false,
                    empty: false,
                },
                LogEntry {
                    commit_id: "c_root".to_string(),
                    change_id: "ch_root".to_string(),
                    author_name: "T".to_string(),
                    author_email: "t@x".to_string(),
                    description: "root".to_string(),
                    description_first_line: "root".to_string(),
                    parents: vec![],
                    local_bookmarks: vec![],
                    remote_bookmarks: vec![],
                    is_working_copy: false,
                    conflict: false,
                    empty: false,
                },
            ],
            merge_source_names: vec![],
        };
        assert_eq!(rebase_root(&seg), "ch_root",
            "must use the oldest change id (changes.last()), not the bookmark tip");
    }

    #[test]
    fn rebase_root_falls_back_to_bookmark_for_empty_segment() {
        // Some segments (e.g., right after the user empties them with
        // `jj abandon`) can have no changes. Fall back to the bookmark's
        // own change id rather than panicking.
        let seg = NarrowedSegment {
            bookmark: Bookmark {
                name: "empty".to_string(),
                commit_id: "c".to_string(),
                change_id: "ch_bookmark".to_string(),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![],
            merge_source_names: vec![],
        };
        assert_eq!(rebase_root(&seg), "ch_bookmark");
    }

    #[test]
    fn block_reasons_emits_local_before_forge() {
        let s = ReconcileState {
            local_failed: true,
            forge_failed: true,
            warnings: vec![],
        };
        let reasons = s.block_reasons();
        assert_eq!(
            reasons,
            vec![BlockReason::LocalSyncFailed, BlockReason::ForgeReconcileFailed],
            "block_reasons must be deterministic and Local-first"
        );
    }

    #[test]
    fn block_reasons_local_only_omits_forge() {
        let s = ReconcileState { local_failed: true, forge_failed: false, warnings: vec![] };
        assert_eq!(s.block_reasons(), vec![BlockReason::LocalSyncFailed]);
    }

    #[test]
    fn block_reasons_forge_only_omits_local() {
        let s = ReconcileState { local_failed: false, forge_failed: true, warnings: vec![] };
        assert_eq!(s.block_reasons(), vec![BlockReason::ForgeReconcileFailed]);
    }

    #[test]
    fn reconcile_state_reset_clears_everything() {
        let mut s = ReconcileState {
            local_failed: true,
            forge_failed: true,
            warnings: vec![LocalDivergenceWarning {
                kind: DivergenceKind::Local,
                message: "x".into(),
            }],
        };
        s.reset();
        assert!(!s.degraded());
        assert!(s.warnings.is_empty());
    }

    /// J: lock the contract that any local-state warning sets local_failed
    /// and any forge-state warning sets forge_failed. The gate's correctness
    /// rests on this — if a future refactor leaks a warning without setting
    /// the flag, the gate goes silently quiet.
    /// A: callers must not re-enter reconcile_after_merge once
    /// state.local_failed is set. The previous else-branch covered that
    /// re-entry; the gate now makes it impossible by construction. Lock
    /// it with a debug_assert and prove the assertion fires.
    #[test]
    #[should_panic(expected = "reconcile_after_merge re-entered with a degraded state")]
    fn reconcile_after_merge_panics_if_re_entered_with_local_failed() {
        struct StubJj;
        impl Jj for StubJj {
            fn git_fetch(&self) -> Result<()> { Ok(()) }
            fn push_bookmark(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn rebase_onto(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".into()) }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".into()) }
            fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> { Ok(vec!["c".into()]) }
            fn merge_into(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn is_conflicted(&self, _: &str) -> Result<bool> { Ok(false) }
        }
        let gh = RecordingGitHub::new().with_evaluatable_pr("auth", 1);
        let plan = MergePlan {
            actions: vec![],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".into(),
            remote_name: "origin".into(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];
        let mut state = ReconcileState {
            local_failed: true,           // simulate caller re-entering
            forge_failed: false,
            warnings: vec![],
        };
        // Should panic in debug builds.
        reconcile_after_merge(&StubJj, &gh, &segments, 0, &plan, ForgeKind::GitHub, &mut state);
    }

    #[test]
    fn reconcile_after_merge_sets_local_failed_when_local_state_warns() {
        struct FailingFetchJj;
        impl Jj for FailingFetchJj {
            fn git_fetch(&self) -> Result<()> { anyhow::bail!("fetch denied") }
            fn push_bookmark(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn rebase_onto(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".into()) }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".into()) }
            fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> {
                Ok(vec!["c".into()])
            }
            fn merge_into(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
            fn is_conflicted(&self, _: &str) -> Result<bool> { Ok(false) }
        }
        let gh = RecordingGitHub::new().with_evaluatable_pr("profile", 2);
        let plan = MergePlan {
            actions: vec![],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".into(),
            remote_name: "origin".into(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];
        let mut state = ReconcileState::default();

        reconcile_after_merge(&FailingFetchJj, &gh, &segments, 0, &plan, ForgeKind::GitHub, &mut state);

        assert!(state.local_failed, "fetch failure must set local_failed");
        assert!(
            state.warnings.iter().any(|w| w.kind == DivergenceKind::Local),
            "fetch failure must record a Local-kind warning"
        );
    }

    #[test]
    fn reconcile_after_merge_sets_forge_failed_when_list_prs_fails() {
        struct ListFailGitHub;
        impl Forge for ListFailGitHub {
            fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
                anyhow::bail!("502 bad gateway")
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
            fn merge_pr(&self, _: &str, _: &str, _: u64, _: MergeMethod) -> Result<()> { Ok(()) }
            fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> { Ok(ChecksStatus::Pass) }
            fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
                Ok(ReviewSummary { approved_count: 1, changes_requested: false })
            }
            fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
                Ok(PrMergeability { mergeable: Some(true), mergeable_state: "clean".into() })
            }
            fn find_merged_pr(&self, _: &str, _: &str, _: &str) -> Result<Option<PullRequest>> { Ok(None) }
            fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".into() })
            }
        }
        let plan = MergePlan {
            actions: vec![],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".into(),
            remote_name: "origin".into(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];
        let jj = RecordingJj::new();
        let mut state = ReconcileState::default();

        reconcile_after_merge(&jj, &ListFailGitHub, &segments, 0, &plan, ForgeKind::GitHub, &mut state);

        assert!(state.forge_failed, "list_open_prs failure must set forge_failed");
        assert!(
            state.warnings.iter().any(|w| w.kind == DivergenceKind::Forge),
            "forge failure must record a Forge-kind warning"
        );
    }

    #[test]
    fn test_stale_plan_does_not_merge_when_ci_now_failing() {
        // The upfront plan says auth is Mergeable (captured when CI was passing).
        // But by execution time, CI has started failing on the forge.
        // The execution should re-evaluate and block — NOT trust the stale plan.
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("auth", 1);
        // Simulate CI failing between plan creation and execution
        gh.checks.insert("sha_auth".to_string(), ChecksStatus::Fail);

        let plan = make_plan_single_mergeable("auth", 1);
        let segments = vec![make_segment("auth")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        // Should NOT have merged — CI is failing
        assert!(
            result.merged.is_empty(),
            "should not merge when CI is now failing: {:?}",
            gh.calls()
        );
        assert!(
            result.blocked_at.is_some(),
            "should be blocked by failing CI"
        );
        let blocked = result.blocked_at.unwrap();
        assert!(
            blocked.reasons.contains(&BlockReason::ChecksFailing),
            "block reason should be ChecksFailing, got: {:?}",
            blocked.reasons
        );
        // merge_pr should never have been called
        assert!(
            !gh.calls().iter().any(|c| c.starts_with("merge_pr")),
            "merge_pr should not be called when CI is failing: {:?}",
            gh.calls()
        );
    }

    #[test]
    fn test_push_failure_blocks_subsequent_merges() {
        // When push_bookmark fails after the first merge, the second PR's
        // local branch never reaches the forge in a rebased state, so
        // proceeding would risk merging it with a bloated diff. The gate
        // must stop further merges and surface LocalSyncFailed.
        let jj = FailingPushJj::new();
        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2)
            .with_evaluatable_pr("settings", 3);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "settings".to_string(),
                    pr: make_pr("settings", 3),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(
            result.merged.len(), 1,
            "only auth should merge before the gate fires: merged={:?}", result.merged
        );
        assert_eq!(result.merged[0].bookmark_name, "auth");
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#1:squash"));
        assert!(
            !gh.calls().iter().any(|c| c == "merge_pr:#2:squash"),
            "profile must not merge while local is degraded"
        );
        assert!(
            !gh.calls().iter().any(|c| c == "merge_pr:#3:squash"),
            "settings must not merge while local is degraded"
        );
        let blocked = result.blocked_at.expect("should be blocked");
        assert_eq!(blocked.bookmark_name, "profile");
        assert!(blocked.reasons.contains(&BlockReason::LocalSyncFailed));
        assert!(
            !result.local_warnings.is_empty(),
            "should report local warnings for push failures"
        );
    }

    #[test]
    fn test_rebase_failure_blocks_subsequent_merges() {
        struct FailingRebaseJj;
        impl Jj for FailingRebaseJj {
            fn git_fetch(&self) -> Result<()> { Ok(()) }
            fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> { Ok(()) }
            fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> {
                anyhow::bail!("rebase failed: conflict")
            }
            fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
            fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
                Ok(vec!["dummy".to_string()])
            }
            fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { Ok(()) }
            fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
        }

        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&FailingRebaseJj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.merged.len(), 1, "only auth should merge");
        let blocked = result.blocked_at.expect("profile should be blocked");
        assert_eq!(blocked.bookmark_name, "profile");
        assert!(blocked.reasons.contains(&BlockReason::LocalSyncFailed));
        assert!(result.local_warnings.iter().any(|w| w.message.contains("rebase")));
    }

    #[test]
    fn test_degraded_blocks_before_second_reconcile() {
        // The gate fires after the first failed reconcile, so the second
        // reconcile is never reached. There should be exactly one
        // `git_fetch` call (from the only reconcile that ran).
        let jj = FailingPushJj::new();
        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2)
            .with_evaluatable_pr("settings", 3);

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "settings".to_string(),
                    pr: make_pr("settings", 3),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();
        assert_eq!(result.merged.len(), 1, "only auth merges before the gate");

        let jj_calls = jj.calls.lock().expect("poisoned");
        let fetch_count = jj_calls.iter().filter(|c| *c == "git_fetch").count();
        assert_eq!(fetch_count, 1, "should only fetch once, not twice: {jj_calls:?}");
    }

    #[test]
    fn test_forge_retarget_still_runs_when_degraded() {
        // Forge-side reconcile (base retarget, stack-comment update) runs
        // inside the same reconcile_after_merge call as local sync, BEFORE
        // the gate decision. So even when local fails and we block further
        // merges, the next PR's base still gets retargeted on the forge —
        // leaving the user's open PR pointing at the right branch.
        let jj = FailingPushJj::new();
        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.open_prs.lock().expect("poisoned")[1]
            .base
            .ref_name = "auth".to_string();

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                PrMergeStatus::Mergeable {
                    bookmark_name: "profile".to_string(),
                    pr: make_pr("profile", 2),
                },
            ],
            repo_info: repo_info(),
            forge_kind: ForgeKind::GitHub,
            options: default_options(),
            default_branch: "main".to_string(),
            remote_name: "origin".to_string(),
            stack_base: None,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false).unwrap();

        assert_eq!(result.merged.len(), 1, "only auth merges; profile gated");
        assert!(
            gh.calls().iter().any(|c| c == "update_base:#2:main"),
            "retarget should run before the gate fires: {:?}",
            gh.calls()
        );
        let blocked = result.blocked_at.expect("profile should be blocked");
        assert!(blocked.reasons.contains(&BlockReason::LocalSyncFailed));
        assert!(!result.local_warnings.is_empty());
    }

    // --- partition_after_merge unit tests ---
    //
    // The reconcile-after-merge path rewrites the stack-info comment on
    // each remaining open PR to reflect that some segments just merged.
    // These tests cover the partition logic in isolation since the
    // full-flow tests use mocks that return empty comments and never
    // exercise the rewrite.

    fn item(name: &str, num: u64, is_merged: bool) -> comment::StackCommentItem {
        comment::StackCommentItem {
            bookmark_name: name.into(),
            pr_url: format!("u_{name}"),
            pr_number: num,
            is_merged,
            closed_at: None,
        }
    }

    fn fossil_item(name: &str, num: u64, closed_at: &str) -> comment::StackCommentItem {
        comment::StackCommentItem {
            bookmark_name: name.into(),
            pr_url: format!("u_{name}"),
            pr_number: num,
            is_merged: true,
            closed_at: Some(closed_at.into()),
        }
    }

    #[test]
    fn test_partition_moves_newly_merged_to_fossils() {
        // Previous comment had A, B, C all live. Merge command merged A.
        // A moves to fossils; B and C stay live.
        let items = vec![item("A", 1, false), item("B", 2, false), item("C", 3, false)];
        let merged: std::collections::HashSet<&str> = ["A"].into_iter().collect();
        let (live, fossils) = partition_after_merge(&items, &merged, "C");
        assert_eq!(
            live.iter().map(|e| e.bookmark_name.as_str()).collect::<Vec<_>>(),
            vec!["B", "C"]
        );
        assert_eq!(
            fossils.iter().map(|e| e.bookmark_name.as_str()).collect::<Vec<_>>(),
            vec!["A"]
        );
        assert!(fossils[0].is_merged, "A must be marked merged");
    }

    #[test]
    fn test_partition_keeps_existing_fossils() {
        // Previous comment had A live, F (already merged in earlier run).
        // No new merges this round.
        let items = vec![item("A", 1, false), fossil_item("F", 2, "2026-04-01T00:00:00Z")];
        let merged: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let (live, fossils) = partition_after_merge(&items, &merged, "A");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].bookmark_name, "A");
        assert_eq!(fossils.len(), 1);
        assert_eq!(fossils[0].bookmark_name, "F");
        // Older fossils preserve their timestamp so the next submit's
        // recency sort stays stable.
        assert_eq!(
            fossils[0].closed_at.as_deref(),
            Some("2026-04-01T00:00:00Z")
        );
    }

    #[test]
    fn test_partition_newly_merged_appears_above_older_fossils() {
        // Layout in previous comment (data.stack order): live entries
        // first in graph order, then fossils sorted by recency. After
        // marking the bottom (A) as merged in this run, the rendered
        // fossil block should naturally show A first (most recent merge),
        // F1 second, F2 third — matching what the next submit will
        // produce when it queries the forge for A's merged_at.
        let items = vec![
            item("A", 1, false),
            item("B", 2, false),
            item("C", 3, false),
            fossil_item("F1", 10, "2026-03-15T00:00:00Z"),
            fossil_item("F2", 11, "2026-02-01T00:00:00Z"),
        ];
        let merged: std::collections::HashSet<&str> = ["A"].into_iter().collect();
        let (_, fossils) = partition_after_merge(&items, &merged, "C");
        assert_eq!(
            fossils.iter().map(|e| e.bookmark_name.as_str()).collect::<Vec<_>>(),
            vec!["A", "F1", "F2"],
            "newly-merged A should land at the top of the fossil block"
        );
        // A has no timestamp yet — next submit will populate from forge.
        assert!(fossils[0].closed_at.is_none());
        // Older fossils keep their stored timestamps unchanged.
        assert_eq!(fossils[1].closed_at.as_deref(), Some("2026-03-15T00:00:00Z"));
    }

    #[test]
    fn test_partition_marks_current_pr_only_among_live() {
        let items = vec![item("A", 1, false), item("B", 2, false), item("C", 3, false)];
        let merged: std::collections::HashSet<&str> = ["A"].into_iter().collect();
        let (live, fossils) = partition_after_merge(&items, &merged, "B");
        let by_name: HashMap<&str, &comment::StackEntry> = live
            .iter()
            .chain(fossils.iter())
            .map(|e| (e.bookmark_name.as_str(), e))
            .collect();
        assert!(by_name["B"].is_current, "B is the current PR");
        assert!(!by_name["A"].is_current, "A is a fossil, never current");
        assert!(!by_name["C"].is_current);
    }

    #[test]
    fn test_partition_does_not_mark_just_merged_as_current() {
        // Edge case: somehow the segment we're commenting on was itself
        // marked merged (shouldn't normally happen since reconcile only
        // iterates remaining-open PRs, but be defensive).
        let items = vec![item("A", 1, false)];
        let merged: std::collections::HashSet<&str> = ["A"].into_iter().collect();
        let (live, fossils) = partition_after_merge(&items, &merged, "A");
        assert!(live.is_empty());
        assert_eq!(fossils.len(), 1);
        assert!(
            !fossils[0].is_current,
            "merged entry must not be flagged as the current PR"
        );
    }

    #[test]
    fn test_partition_empty_input() {
        let merged: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let (live, fossils) = partition_after_merge(&[], &merged, "anything");
        assert!(live.is_empty());
        assert!(fossils.is_empty());
    }
}
