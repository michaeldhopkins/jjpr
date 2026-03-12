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

/// Post-merge reconciliation: fetch, check divergence, rebase, push, retarget.
///
/// Called after every successful merge (both normal and watch paths) when
/// there are remaining segments in the stack.
fn reconcile_after_merge(
    jj: &dyn Jj,
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    seg_idx: usize,
    owner: &str,
    repo: &str,
    effective_base: &str,
    remote_name: &str,
    fk: ForgeKind,
) -> Result<HashMap<String, PullRequest>> {
    println!("  Fetching remotes...");
    jj.git_fetch().context("failed to fetch after merge")?;

    let next_segment = &segments[seg_idx + 1];
    let next_change_id = &next_segment.bookmark.change_id;

    // Check for divergent change IDs before rebasing
    match jj.resolve_change_id(next_change_id) {
        Ok(ref commit_ids) if commit_ids.len() > 1 => {
            let short_id = &next_change_id[..next_change_id.len().min(12)];
            let count = commit_ids.len();
            anyhow::bail!(
                "change '{short_id}' is divergent ({count} commits share this change ID).\n\n\
                 The merge succeeded on the forge, but the local rebase can't\n\
                 proceed because jj doesn't know which commit to rebase.\n\n\
                 To fix:\n  \
                   jj log -r 'all:{next_change_id}'   # see the divergent commits\n  \
                   jj abandon <stale_commit_id>        # remove the stale one\n  \
                   jjpr merge                          # re-run to continue",
            );
        }
        Ok(commit_ids) if commit_ids.is_empty() => {
            anyhow::bail!(
                "change ID '{next_change_id}' not found in the local repository.\n\
                 The merge succeeded on the forge but the local state is out of sync."
            );
        }
        Err(e) => {
            eprintln!("  Warning: could not verify change ID: {e}");
        }
        _ => {}
    }

    println!("  Rebasing remaining stack onto {effective_base}...");
    jj.rebase_onto(next_change_id, effective_base)
        .context("failed to rebase remaining stack")?;

    for seg in &segments[seg_idx + 1..] {
        println!("  Pushing '{}'...", seg.bookmark.name);
        jj.push_bookmark(&seg.bookmark.name, remote_name)
            .with_context(|| format!("failed to push '{}'", seg.bookmark.name))?;
    }

    // Refresh PR state after merge
    let fresh_prs = forge.list_open_prs(owner, repo)?;
    let fresh_map = crate::forge::build_pr_map(fresh_prs, owner);

    // Retarget next PR if its base still points at the merged branch
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

/// Result of executing a merge plan.
#[derive(Debug)]
pub struct MergeResult {
    pub merged: Vec<MergedPr>,
    pub blocked_at: Option<BlockedPr>,
    pub skipped_merged: Vec<SkippedMergedPr>,
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
    let effective_base = plan.stack_base.as_deref().unwrap_or(&plan.default_branch);
    let fk = plan.forge_kind;

    let mut merged = Vec::new();
    let mut blocked_at = None;
    let mut skipped_merged = Vec::new();

    // Before any merge, trust the upfront plan.
    // After a merge, re-evaluate remaining segments against live GitHub state.
    let mut pr_map: Option<HashMap<String, PullRequest>> = None;

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
                        jj, github, segments, seg_idx, owner, repo,
                        effective_base, &plan.remote_name, fk,
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
                                jj, github, segments, seg_idx, owner, repo,
                                effective_base, &plan.remote_name, fk,
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
    }

    fn default_options() -> MergeOptions {
        MergeOptions {
            merge_method: MergeMethod::Squash,
            required_approvals: 1,
            require_ci_pass: true,
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
        let gh = RecordingGitHub::new();
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
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("profile", 2);
        gh.checks.insert("profile".to_string(), ChecksStatus::Pending);
        // Profile's base points at auth (needs retargeting)
        gh.open_prs.lock().expect("poisoned")[0]
            .base
            .ref_name = "auth".to_string();

        let plan = MergePlan {
            actions: vec![
                PrMergeStatus::Mergeable {
                    bookmark_name: "auth".to_string(),
                    pr: make_pr("auth", 1),
                },
                // Second action is not used after merge — re-evaluation takes over
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
    }

    #[test]
    fn test_no_retarget_when_base_already_correct() {
        let jj = RecordingJj::new();
        // Profile PR's base is already "main" (the default from make_pr)
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("profile", 2);
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

        execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

        // Should NOT call update_base since it's already "main"
        assert!(
            !gh.calls().iter().any(|c| c.starts_with("update_base")),
            "should not retarget when base is already correct: {:?}",
            gh.calls()
        );
    }

    #[test]
    fn test_push_uses_plan_remote_name() {
        let jj = RecordingJj::new();
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("profile", 2);
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

        execute_merge_plan(&jj, &gh, &plan, &segments, false, false).unwrap();

        assert!(
            jj.calls().iter().any(|c| c == "push:profile:upstream"),
            "should push to the remote from the plan, not hardcoded origin: {:?}",
            jj.calls()
        );
    }

    #[test]
    fn test_already_merged_skipped() {
        let jj = RecordingJj::new();
        let gh = RecordingGitHub::new();

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
        let gh = RecordingGitHub::new();

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
        let mut gh = RecordingGitHub::new().with_evaluatable_pr("profile", 2);
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

        let err = execute_merge_plan(&DivergentJj, &gh, &plan, &segments, false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("divergent"), "should mention divergence: {msg}");
        assert!(msg.contains("2 commits"), "should mention count: {msg}");
        // The merge of auth should have succeeded (merge_pr was called)
        assert!(gh.calls().iter().any(|c| c == "merge_pr:#1:squash"));
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
}
