use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::forge::http::HttpError;
use crate::forge::types::{MergeMethod, PullRequest};
use crate::forge::{Forge, ForgeKind};
use crate::jj::types::NarrowedSegment;
use crate::jj::Jj;

use super::plan::{evaluate_segment, BlockReason, MergePlan, PrMergeStatus};

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

    println!("  Fetching remotes...");
    if let Err(e) = jj.git_fetch() {
        eprintln!("  Warning: failed to fetch: {e}");
        warnings.push(LocalDivergenceWarning {
            message: format!("Failed to fetch remotes: {e}"),
        });
        return warnings;
    }

    // Track which bookmarks to push. With merge strategy, only push bookmarks
    // whose merge_into succeeded (to avoid pushing stale state).
    let bookmarks_to_push: Vec<&str> = match strategy {
        crate::config::ReconcileStrategy::Merge => {
            // Merge-based sync: create merge commits incorporating the new base.
            // This is append-only — pushes are fast-forward (no force push).
            println!("  Syncing remaining stack with {effective_base}...");
            let mut succeeded = Vec::new();
            for seg in &segments[seg_idx + 1..] {
                if let Err(e) = jj.merge_into(&seg.bookmark.name, effective_base) {
                    eprintln!("  Warning: merge sync failed for '{}': {e}", seg.bookmark.name);
                    warnings.push(LocalDivergenceWarning {
                        message: format!("Failed to merge-sync '{}': {e}", seg.bookmark.name),
                    });
                } else {
                    succeeded.push(seg.bookmark.name.as_str());
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
                    warnings.push(LocalDivergenceWarning {
                        message: format!(
                            "Change '{short_id}' is divergent ({count} commits share this change ID)"
                        ),
                    });
                    return warnings;
                }
                Ok(commit_ids) if commit_ids.is_empty() => {
                    warnings.push(LocalDivergenceWarning {
                        message: format!(
                            "Change ID '{next_change_id}' not found locally"
                        ),
                    });
                    return warnings;
                }
                Err(e) => {
                    eprintln!("  Warning: could not verify change ID: {e}");
                }
                _ => {}
            }

            // Rebase from the oldest commit in the next segment — not the bookmark tip.
            let rebase_root = next_segment
                .changes
                .last()
                .map(|c| c.change_id.as_str())
                .unwrap_or(next_change_id);

            println!("  Rebasing remaining stack onto {effective_base}...");
            if let Err(e) = jj.rebase_onto(rebase_root, effective_base) {
                eprintln!("  Warning: rebase failed: {e}");
                warnings.push(LocalDivergenceWarning {
                    message: format!("Failed to rebase remaining stack: {e}"),
                });
                return warnings;
            }

            // Rebase succeeded — push all remaining bookmarks
            segments[seg_idx + 1..].iter().map(|s| s.bookmark.name.as_str()).collect()
        }
    };

    for name in &bookmarks_to_push {
        println!("  Pushing '{name}'...");
        if let Err(e) = jj.push_bookmark(name, remote_name) {
            eprintln!("  Warning: failed to push '{name}': {e}");
            warnings.push(LocalDivergenceWarning {
                message: format!("Failed to push '{name}': {e}"),
            });
        }
    }

    warnings
}

/// Refresh PR state from forge and retarget the next PR's base if needed.
///
/// Independent of local state — runs even when local reconciliation failed.
fn reconcile_forge_state(
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    seg_idx: usize,
    owner: &str,
    repo: &str,
    effective_base: &str,
    fk: ForgeKind,
) -> Result<HashMap<String, PullRequest>> {
    let fresh_prs = forge.list_open_prs(owner, repo)?;
    let fresh_map = crate::forge::build_pr_map(fresh_prs, owner);

    let next_name = &segments[seg_idx + 1].bookmark.name;
    if let Some(next_pr) = fresh_map.get(next_name)
        && next_pr.base.ref_name != effective_base
    {
        println!(
            "  Updating {} base to '{effective_base}'...",
            fk.format_ref(next_pr.number)
        );
        forge.update_pr_base(owner, repo, next_pr.number, effective_base)?;
    }

    Ok(fresh_map)
}

/// Run both local and forge reconciliation after a successful merge.
///
/// Shared by the normal merge path and the watch-mode path to avoid duplication.
fn reconcile_after_merge(
    jj: &dyn Jj,
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    seg_idx: usize,
    plan: &MergePlan,
    fk: ForgeKind,
    local_degraded: &mut bool,
    local_warnings: &mut Vec<LocalDivergenceWarning>,
) -> Result<HashMap<String, PullRequest>> {
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;
    let effective_base = plan.stack_base.as_deref().unwrap_or(&plan.default_branch);

    if !*local_degraded {
        let warnings = reconcile_local_state(
            jj, segments, seg_idx, effective_base, &plan.remote_name,
            plan.options.reconcile_strategy,
        );
        if !warnings.is_empty() {
            *local_degraded = true;
            local_warnings.extend(warnings);
        }
    } else {
        println!("  Skipping local sync (local state already diverged)");
    }
    reconcile_forge_state(forge, segments, seg_idx, owner, repo, effective_base, fk)
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

/// A warning about local state being out of sync with the forge.
#[derive(Debug)]
pub struct LocalDivergenceWarning {
    pub message: String,
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
    watch: bool,
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
    let mut local_warnings: Vec<LocalDivergenceWarning> = Vec::new();
    let mut local_degraded = false;

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

        match status {
            PrMergeStatus::AlreadyMerged {
                bookmark_name,
                pr_number,
            } => {
                println!(
                    "  Skipping '{bookmark_name}' \u{2014} {} already merged",
                    fk.format_ref(pr_number)
                );
                skipped_merged.push(SkippedMergedPr {
                    bookmark_name,
                    pr_number,
                });
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

                if seg_idx + 1 < segments.len() {
                    let fresh_map = reconcile_after_merge(
                        jj, github, segments, seg_idx, plan, fk,
                        &mut local_degraded, &mut local_warnings,
                    )?;
                    pr_map = Some(fresh_map);
                }
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

                // Watch mode: poll transient blockers until resolved or timeout
                if watch && reasons.iter().all(|r| r.is_transient()) {
                    // Build a fresh PR map if we don't have one yet (first segment)
                    let watch_map = if let Some(ref map) = pr_map {
                        map.clone()
                    } else {
                        let fresh_prs = github.list_open_prs(owner, repo)?;
                        crate::forge::build_pr_map(fresh_prs, owner)
                    };
                    {
                        println!("\n  Watching... (polling every 30s, timeout 30m)");
                        let deadline = Instant::now() + Duration::from_secs(30 * 60);
                        let mut resolved = false;
                        while Instant::now() < deadline {
                            thread::sleep(Duration::from_secs(30));
                            match evaluate_segment(
                                github,
                                &bookmark_name,
                                &plan.repo_info,
                                &watch_map,
                                &plan.options,
                            ) {
                                Ok(PrMergeStatus::Mergeable { bookmark_name: bm, pr: p }) => {
                                    println!("  Ready \u{2014} continuing merge.");
                                    // Re-inject as mergeable and let the outer loop handle it
                                    // For simplicity, just do the merge inline here
                                    println!(
                                        "\n  Merging '{bm}' ({}, {})...",
                                        fk.format_ref(p.number), plan.options.merge_method
                                    );
                                    println!("    {}", p.html_url);
                                    merge_with_retry(
                                        github, owner, repo, p.number, plan.options.merge_method, fk,
                                    )
                                    .with_context(|| {
                                        format!("failed to merge {} for '{bm}'", fk.format_ref(p.number))
                                    })?;
                                    merged.push(MergedPr {
                                        bookmark_name: bm,
                                        pr_number: p.number,
                                        html_url: p.html_url.clone(),
                                    });
                                    resolved = true;
                                    break;
                                }
                                Ok(PrMergeStatus::Blocked { reasons: new_reasons, .. })
                                    if new_reasons.iter().all(|r| r.is_transient()) =>
                                {
                                    print!(".");
                                    use std::io::Write;
                                    std::io::stdout().flush().ok();
                                    continue;
                                }
                                Ok(PrMergeStatus::Blocked { reasons: new_reasons, pr: new_pr, bookmark_name: bm }) => {
                                    println!("\n  Blocked at '{bm}' (no longer transient):");
                                    for reason in &new_reasons {
                                        println!("    - {}", format_block_reason(reason, fk));
                                    }
                                    blocked_at = Some(BlockedPr {
                                        bookmark_name: bm,
                                        pr_number: new_pr.as_ref().map(|p| p.number),
                                        reasons: new_reasons,
                                    });
                                    break;
                                }
                                Ok(PrMergeStatus::AlreadyMerged { bookmark_name: bm, pr_number: pn }) => {
                                    println!("\n  '{bm}' was merged externally.");
                                    skipped_merged.push(SkippedMergedPr {
                                        bookmark_name: bm,
                                        pr_number: pn,
                                    });
                                    resolved = true;
                                    break;
                                }
                                Err(e) => {
                                    eprintln!("\n  Watch poll error: {e}");
                                    blocked_at = Some(BlockedPr {
                                        bookmark_name: bookmark_name.clone(),
                                        pr_number: pr.as_ref().map(|p| p.number),
                                        reasons: reasons.clone(),
                                    });
                                    break;
                                }
                            }
                        }
                        if !resolved && blocked_at.is_none() {
                            println!("\n  Watch timed out after 30 minutes.");
                            blocked_at = Some(BlockedPr {
                                bookmark_name,
                                pr_number: pr.as_ref().map(|p| p.number),
                                reasons,
                            });
                        }
                        if resolved && seg_idx + 1 < segments.len() {
                            let fresh_map = reconcile_after_merge(
                                jj, github, segments, seg_idx, plan, fk,
                                &mut local_degraded, &mut local_warnings,
                            )?;
                            pr_map = Some(fresh_map);
                        }
                        if resolved {
                            continue;
                        }
                    }
                } else {
                    blocked_at = Some(BlockedPr {
                        bookmark_name,
                        pr_number: pr.as_ref().map(|p| p.number),
                        reasons,
                    });
                }
                break;
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

/// Attempt to merge a PR with retry logic for transient HTTP errors.
///
/// Handles:
/// - 502/503: transient server errors — verify state, then retry
/// - 405 "already in progress": GitHub is processing — poll until merged
/// - Other errors: propagate immediately
fn merge_with_retry(
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
                                "merge of {} still in progress after 30s — check the forge manually",
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
                    "  Skipping '{bookmark_name}' \u{2014} {} already merged",
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

fn format_block_reason(reason: &BlockReason, fk: ForgeKind) -> String {
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
            },
            head: PullRequestRef {
                ref_name: name.to_string(),
                label: String::new(),
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
                .insert(name.to_string(), ChecksStatus::Pass);
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
        fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
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
    }

    fn default_options() -> MergeOptions {
        MergeOptions {
            merge_method: MergeMethod::Squash,
            required_approvals: 1,
            require_ci_pass: true,
            reconcile_strategy: crate::config::ReconcileStrategy::Rebase,
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
        }
    }

    #[test]
    fn test_dry_run_no_api_calls() {
        let jj = RecordingJj::new();
        let gh = RecordingGitHub::new();
        let plan = make_plan_single_mergeable("auth", 1);
        let segments = vec![make_segment("auth")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, true, false).unwrap();

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

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
    fn test_rebase_uses_oldest_commit_in_segment() {
        // When a segment has multiple commits (e.g., 3 commits between two bookmarks),
        // the rebase must start from the oldest commit (closest to the merged bookmark),
        // not the bookmark tip. Otherwise intermediate commits are orphaned.
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
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
                },
            ],
            merge_source_names: vec![],
        };
        let segments = vec![make_segment("auth"), profile_segment];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();
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
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();
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
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();
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
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
        gh.checks.insert("settings".to_string(), ChecksStatus::Pending);
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
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();
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
        }

        let jj = FailingMergeJj::new();
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2)
            .with_evaluatable_pr("settings", 3);
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
        gh.checks.insert("settings".to_string(), ChecksStatus::Pending);
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
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

        let jj_calls = jj.calls();
        // merge_into attempted for both
        assert!(jj_calls.iter().any(|c| c == "merge_into:profile"));
        assert!(jj_calls.iter().any(|c| c == "merge_into:settings"));
        // profile failed → should NOT be pushed; settings succeeded → should be pushed
        assert!(
            !jj_calls.iter().any(|c| c == "push:profile:origin"),
            "should NOT push bookmark whose merge_into failed: {jj_calls:?}"
        );
        assert!(
            jj_calls.iter().any(|c| c == "push:settings:origin"),
            "should push bookmark whose merge_into succeeded: {jj_calls:?}"
        );
        // Should have warnings about the failure
        assert!(!result.local_warnings.is_empty());
    }

    #[test]
    fn test_no_retarget_when_base_already_correct() {
        let jj = RecordingJj::new();
        // Profile PR's base is already "main" (the default from make_pr)
        let mut gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);

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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);

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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

        assert_eq!(result.skipped_merged.len(), 1);
        assert_eq!(result.skipped_merged[0].pr_number, 1);
        assert_eq!(result.merged.len(), 1);
        assert_eq!(result.merged[0].pr_number, 2);
    }

    #[test]
    fn test_blocked_stops_execution() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("auth", 1);
        // Make auth a draft with failing CI so it blocks
        gh.open_prs.lock().expect("poisoned")[0].draft = true;
        gh.checks.insert("auth".to_string(), ChecksStatus::Fail);

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
        };
        let segments = vec![make_segment("auth")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
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

        let err = execute_merge_plan(&jj, &FailingMergeGitHub, &plan, &segments, false, false).unwrap_err();
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
            .insert("profile".to_string(), ChecksStatus::Pending);

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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
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
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
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
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
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
    fn test_divergent_change_id_detected() {
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&DivergentJj, &gh, &plan, &segments, false, false).unwrap();

        // Both PRs should merge on the forge despite local divergence
        assert_eq!(result.merged.len(), 2, "both PRs should merge: {:?}", result.merged);
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#1:squash"));
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#2:squash"));

        // Should report divergence as a local warning, not an error
        assert!(
            result.local_warnings.iter().any(|w| w.message.contains("divergent")),
            "should warn about divergence: {:?}", result.local_warnings
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
    }

    #[test]
    fn test_stale_plan_does_not_merge_when_ci_now_failing() {
        // The upfront plan says auth is Mergeable (captured when CI was passing).
        // But by execution time, CI has started failing on the forge.
        // The execution should re-evaluate and block — NOT trust the stale plan.
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("auth", 1);
        // Simulate CI failing between plan creation and execution
        gh.checks.insert("auth".to_string(), ChecksStatus::Fail);

        let plan = make_plan_single_mergeable("auth", 1);
        let segments = vec![make_segment("auth")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

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
    fn test_push_failure_continues_merging() {
        // When push_bookmark fails (e.g., conflicted commits from local divergence),
        // jjpr should continue merging remaining PRs on the forge and report
        // local warnings instead of hard-failing.
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
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

        // All 3 PRs should have been merged on the forge
        assert_eq!(
            result.merged.len(), 3,
            "all PRs should merge despite push failure: merged={:?}, blocked={:?}",
            result.merged, result.blocked_at
        );
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#1:squash"));
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#2:squash"));
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#3:squash"));

        // Should have local warnings about push failures
        assert!(
            !result.local_warnings.is_empty(),
            "should report local warnings for push failures"
        );
    }

    #[test]
    fn test_rebase_failure_continues_merging() {
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&FailingRebaseJj, &gh, &plan, &segments, false, false).unwrap();

        assert_eq!(result.merged.len(), 2);
        assert!(result.local_warnings.iter().any(|w| w.message.contains("rebase")));
    }

    #[test]
    fn test_degraded_skips_subsequent_local_ops() {
        // After first reconciliation fails, subsequent ones should skip local ops.
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
        };
        let segments = vec![
            make_segment("auth"),
            make_segment("profile"),
            make_segment("settings"),
        ];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();
        assert_eq!(result.merged.len(), 3);

        // Should only have attempted fetch+rebase+push once (for the first reconciliation).
        // The second reconciliation should be skipped entirely.
        let jj_calls = jj.calls.lock().expect("poisoned");
        let fetch_count = jj_calls.iter().filter(|c| *c == "git_fetch").count();
        assert_eq!(fetch_count, 1, "should only fetch once, not twice: {jj_calls:?}");
    }

    #[test]
    fn test_forge_retarget_still_runs_when_degraded() {
        let jj = FailingPushJj::new();
        let gh = RecordingGitHub::new()
            .with_evaluatable_pr("auth", 1)
            .with_evaluatable_pr("profile", 2);
        // Profile's base points at auth (needs retargeting to main after auth merges)
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
        };
        let segments = vec![make_segment("auth"), make_segment("profile")];

        let result = execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

        // Both should merge despite push failure
        assert_eq!(result.merged.len(), 2);

        // Forge retarget should still happen
        assert!(
            gh.calls().iter().any(|c| c == "update_base:#2:main"),
            "should retarget profile PR even when local is degraded: {:?}",
            gh.calls()
        );

        // Should have local warnings
        assert!(!result.local_warnings.is_empty());
    }
}
