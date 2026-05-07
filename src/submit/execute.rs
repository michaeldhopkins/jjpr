use std::collections::HashMap;

use anyhow::Result;

use crate::forge::comment::{self, StackEntry};
use crate::forge::types::PullRequest;
use crate::forge::Forge;
use crate::jj::Jj;

use super::plan::SubmissionPlan;

/// Execute the submission plan: push, create PRs, update bases, manage comments.
pub fn execute_submission_plan(
    jj: &dyn Jj,
    github: &dyn Forge,
    plan: &SubmissionPlan,
    reviewers: &[String],
    dry_run: bool,
) -> Result<()> {
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;
    let fk = plan.forge_kind;
    let mut completed_actions: Vec<String> = Vec::new();

    // Report merged bookmarks
    for item in &plan.bookmarks_already_merged {
        println!(
            "  Skipping '{}': {} already merged",
            item.bookmark.name, fk.format_ref(item.pr_number)
        );
    }

    // Phase 1: Push bookmarks
    for bookmark in &plan.bookmarks_needing_push {
        if dry_run {
            println!("  Would push bookmark '{}' to {}", bookmark.name, plan.remote_name);
            continue;
        }
        println!("  Pushing '{}'...", bookmark.name);
        if let Err(e) = jj.push_bookmark(&bookmark.name, &plan.remote_name) {
            report_partial_failure(&completed_actions);
            return Err(e);
        }
        completed_actions.push(format!("Pushed '{}'", bookmark.name));

        // Show PR URL if this bookmark has an existing PR, and verify it
        // wasn't auto-closed by the force-push (GitHub closes PRs when head
        // is no longer ahead of base).
        if let Some(pr) = plan.existing_prs.get(&bookmark.name) {
            println!("    {}", pr.html_url);
            if let Ok(state) = github.get_pr_state(owner, repo, pr.number)
                && state.state == "closed" && !state.merged
            {
                eprintln!(
                    "\n  Warning: {} was closed after pushing '{}'.",
                    fk.format_ref(pr.number), bookmark.name
                );
                eprintln!(
                    "    This means the bookmark's changes are already in the base branch."
                );
                eprintln!(
                    "    hint: jj bookmark delete {} && jj git push --deleted",
                    bookmark.name
                );
            }
        }
    }

    // Phase 2: Create new PRs
    let mut bookmark_to_pr: HashMap<String, PullRequest> = plan.existing_prs.clone();

    for item in &plan.bookmarks_needing_pr {
        if dry_run {
            println!(
                "  Would create {} for '{}' (base: {})",
                fk.request_abbreviation(), item.bookmark.name, item.base_branch
            );
            continue;
        }
        let label = if plan.draft { " (draft)" } else { "" };
        println!("  Creating {}{label} for '{}'...", fk.request_abbreviation(), item.bookmark.name);
        let pr = match github.create_pr(
            owner,
            repo,
            &item.title,
            &item.body,
            &item.bookmark.name,
            &item.base_branch,
            plan.draft,
        ) {
            Ok(pr) => pr,
            Err(e) => {
                report_partial_failure(&completed_actions);
                return Err(e);
            }
        };
        println!("    {}", pr.html_url);
        completed_actions.push(format!("Created {} for '{}'", fk.format_ref(pr.number), item.bookmark.name));

        // Request reviewers on new PRs
        if !reviewers.is_empty()
            && let Err(e) = github.request_reviewers(owner, repo, pr.number, reviewers)
        {
            report_partial_failure(&completed_actions);
            return Err(e);
        }

        bookmark_to_pr.insert(item.bookmark.name.clone(), pr);
    }

    // Phase 3: Update PR bases
    for item in &plan.bookmarks_needing_base_update {
        if dry_run {
            println!(
                "  Would update {} base: {} -> {}",
                fk.format_ref(item.pr.number), item.pr.base.ref_name, item.expected_base
            );
            continue;
        }
        println!(
            "  Updating {} base to '{}'...",
            fk.format_ref(item.pr.number), item.expected_base
        );
        if let Err(e) = github.update_pr_base(owner, repo, item.pr.number, &item.expected_base) {
            report_partial_failure(&completed_actions);
            return Err(e);
        }
        completed_actions.push(format!("Updated {} base to '{}'", fk.format_ref(item.pr.number), item.expected_base));
    }

    // Phase 4: Update stale PR bodies
    for item in &plan.bookmarks_needing_body_update {
        if dry_run {
            println!(
                "  Would update {} body for '{}'",
                fk.format_ref(item.pr_number), item.bookmark.name
            );
            continue;
        }
        println!(
            "  Updating {} body for '{}'...",
            fk.format_ref(item.pr_number), item.bookmark.name
        );
        if let Err(e) = github.update_pr_body(owner, repo, item.pr_number, &item.new_body) {
            report_partial_failure(&completed_actions);
            return Err(e);
        }
        completed_actions.push(format!("Updated {} body", fk.format_ref(item.pr_number)));
    }

    // Phase 5: Convert draft PRs to ready
    for item in &plan.bookmarks_needing_ready {
        if dry_run {
            println!(
                "  Would mark {} as ready for review ('{}')",
                fk.format_ref(item.pr_number), item.bookmark.name
            );
            continue;
        }
        println!(
            "  Marking {} as ready for review ('{}')...",
            fk.format_ref(item.pr_number), item.bookmark.name
        );
        if let Err(e) = github.mark_pr_ready(owner, repo, item.pr_number) {
            report_partial_failure(&completed_actions);
            return Err(e);
        }
        completed_actions.push(format!("Marked {} as ready", fk.format_ref(item.pr_number)));
    }

    // Phase 6: Request reviewers on existing PRs (skip already-requested)
    for (bookmark, pr_number) in &plan.bookmarks_needing_reviewers {
        let already_requested: &[String] = plan
            .existing_prs
            .get(&bookmark.name)
            .map(|pr| pr.requested_reviewers.as_slice())
            .unwrap_or_default();
        if reviewers
            .iter()
            .all(|r| already_requested.iter().any(|a| a.eq_ignore_ascii_case(r)))
        {
            continue;
        }
        if dry_run {
            println!(
                "  Would request reviewers on {} ('{}')",
                fk.format_ref(*pr_number), bookmark.name
            );
            continue;
        }
        println!(
            "  Requesting reviewers on {}...",
            fk.format_ref(*pr_number)
        );
        // Full desired set: GitLab's PUT replaces the reviewer list,
        // so we include existing reviewers to avoid dropping them.
        // GitHub/Forgejo use additive POST, so duplicates are harmless.
        let mut all_reviewers: Vec<String> = already_requested.to_vec();
        for r in reviewers {
            if !all_reviewers.iter().any(|a| a.eq_ignore_ascii_case(r)) {
                all_reviewers.push(r.clone());
            }
        }
        if let Err(e) = github.request_reviewers(owner, repo, *pr_number, &all_reviewers) {
            report_partial_failure(&completed_actions);
            return Err(e);
        }
        completed_actions.push(format!("Requested reviewers on {}", fk.format_ref(*pr_number)));
    }

    // Phase 7: Update/create stack navigation on all PRs
    let nav = comment::create_stack_nav(plan.stack_nav);
    let comments_updated = if dry_run {
        println!("  Would update stack comments");
        0
    } else {
        match update_stack_comments(github, nav.as_ref(), plan, &bookmark_to_pr) {
            Ok(n) => {
                if n > 0 {
                    println!("  Updated stack comments on {n} {}.", if n == 1 { "PR" } else { "PRs" });
                }
                n
            }
            Err(e) => {
                eprintln!("  Warning: failed to update stack comments: {e}");
                eprintln!("  (run `jjpr submit` again to retry)");
                0
            }
        }
    };

    // Report title drift
    print_title_drift_warnings(&plan.bookmarks_with_title_drift, &plan.repo_info, fk);

    if !plan.has_actions() && plan.bookmarks_already_merged.is_empty() && comments_updated == 0 {
        println!("  Stack is up to date.");
    }

    Ok(())
}

fn print_title_drift_warnings(
    drifts: &[super::plan::TitleDrift],
    repo_info: &crate::forge::types::RepoInfo,
    forge_kind: crate::forge::ForgeKind,
) {
    for drift in drifts {
        let escaped_title = drift.expected_title.replace('\'', "'\\''");
        let fix_hint = match forge_kind {
            crate::forge::ForgeKind::GitHub | crate::forge::ForgeKind::Forgejo => format!(
                "gh pr edit {} --repo {}/{} --title '{}'",
                drift.pr_number, repo_info.owner, repo_info.repo, escaped_title,
            ),
            crate::forge::ForgeKind::GitLab => format!(
                "glab mr update {} --title '{}'",
                drift.pr_number, escaped_title,
            ),
        };
        println!(
            "  Note: {} title differs from commit description\n\
             \x20        current: \"{}\"\n\
             \x20        expected: \"{}\"\n\
             \x20        fix with: {fix_hint}",
            forge_kind.format_ref(drift.pr_number),
            drift.current_title,
            drift.expected_title,
        );
    }
}

fn report_partial_failure(completed: &[String]) {
    if !completed.is_empty() {
        eprintln!("\nThe following actions completed before the error:");
        for action in completed {
            eprintln!("  - {action}");
        }
        eprintln!();
    }
}

/// Visible for testing only — not part of the public API.
/// A stack entry with its merged status, used as intermediate representation.
#[derive(Clone)]
struct EntryData {
    name: String,
    url: Option<String>,
    number: Option<u64>,
    is_merged: bool,
    /// ISO-8601 timestamp from the forge marking when the PR became
    /// non-open. Sourced from `find_merged_pr` for current entries and
    /// inherited from the previous comment for entries no longer in the
    /// local stack.
    closed_at: Option<String>,
}

/// Classify entries for rendering, returning `(live, fossils)`.
///
/// Live entries are open PRs in the current local stack, ordered base→top
/// to match the live jj graph. Fossils are closed/merged PRs (either
/// still in the local stack as merged, or known only from the previous
/// comment); they are sorted by `closed_at` descending — most recent
/// first — and rendered inside a collapsible block at the bottom.
///
/// Entries appearing only in `previous` (no longer in the local jj
/// graph) are inherited as fossils with `is_merged: true`. Their
/// `closed_at` comes from the persisted JJPR_DATA payload, so a
/// developer running submit after another developer keeps the same
/// chronology without needing to re-query the forge.
fn classify_stack_entries(
    current: &[EntryData],
    previous: &[comment::StackCommentItem],
) -> (Vec<EntryData>, Vec<EntryData>) {
    use std::collections::HashSet;

    let previous_by_name: HashMap<&str, &comment::StackCommentItem> = previous
        .iter()
        .map(|p| (p.bookmark_name.as_str(), p))
        .collect();

    let mut live = Vec::new();
    let mut fossils = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Walk current_entries in graph order. Live entries take their position
    // from the current local graph; fossils are deferred to the fossil
    // bucket and sorted later by timestamp.
    for entry in current {
        seen.insert(entry.name.clone());
        if entry.is_merged {
            // Backfill closed_at from previous if the current EntryData is
            // missing it (e.g., transient forge error or the bookmark was
            // marked merged in a prior submit).
            let closed_at = entry.closed_at.clone().or_else(|| {
                previous_by_name
                    .get(entry.name.as_str())
                    .and_then(|p| p.closed_at.clone())
            });
            fossils.push(EntryData {
                closed_at,
                ..entry.clone()
            });
        } else {
            live.push(entry.clone());
        }
    }

    // Previous entries no longer in the local graph become fossils.
    for prev in previous {
        if seen.contains(&prev.bookmark_name) {
            continue;
        }
        fossils.push(EntryData {
            name: prev.bookmark_name.clone(),
            url: Some(prev.pr_url.clone()),
            number: Some(prev.pr_number),
            is_merged: true,
            closed_at: prev.closed_at.clone(),
        });
    }

    // Sort fossils by closed_at descending. Entries with a timestamp sort
    // before timestampless ones; within timestamped, newer first; within
    // timestampless, preserve insertion order (a stable sort plus None
    // comparing equal handles this).
    fossils.sort_by(|a, b| match (&a.closed_at, &b.closed_at) {
        (Some(a_t), Some(b_t)) => b_t.cmp(a_t),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    (live, fossils)
}

pub(crate) fn update_stack_comments(
    forge: &dyn Forge,
    nav: &dyn comment::StackNav,
    plan: &SubmissionPlan,
    bookmark_to_pr: &HashMap<String, PullRequest>,
) -> Result<usize> {
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;
    let mut updated = 0;

    // Count bookmarks in the stack (excluding default branch) — skip stack nav
    // for single-bookmark stacks so they look like normal PRs to reviewers.
    let stack_bookmark_count = plan
        .all_bookmarks
        .iter()
        .filter(|b| b.name != plan.default_branch)
        .count();

    // Build a lookup for merged PRs so their links are preserved
    let merged_prs: HashMap<&str, &super::plan::MergedBookmark> = plan
        .bookmarks_already_merged
        .iter()
        .map(|m| (m.bookmark.name.as_str(), m))
        .collect();

    // Current entries from this submission's segments
    let current_entries: Vec<EntryData> = plan
        .all_bookmarks
        .iter()
        .filter(|b| b.name != plan.default_branch)
        .map(|b| {
            if let Some(pr) = bookmark_to_pr.get(&b.name) {
                EntryData {
                    name: b.name.clone(),
                    url: Some(pr.html_url.clone()),
                    number: Some(pr.number),
                    is_merged: false,
                    closed_at: None,
                }
            } else if let Some(merged) = merged_prs.get(b.name.as_str()) {
                EntryData {
                    name: b.name.clone(),
                    url: Some(merged.html_url.clone()),
                    number: Some(merged.pr_number),
                    is_merged: true,
                    closed_at: merged.merged_at.clone(),
                }
            } else {
                EntryData {
                    name: b.name.clone(),
                    url: None,
                    number: None,
                    is_merged: false,
                    closed_at: None,
                }
            }
        })
        .collect();

    for bookmark in plan.all_bookmarks.iter().filter(|b| b.name != plan.default_branch) {
        let Some(pr) = bookmark_to_pr.get(&bookmark.name) else {
            continue;
        };

        // For single-bookmark stacks: skip creating new nav but keep updating existing ones
        if stack_bookmark_count <= 1 && !nav.has_existing(forge, owner, repo, pr)? {
            continue;
        }

        let bookmark_name = bookmark.name.clone();
        let did_update = nav.update(forge, owner, repo, pr, &|previous_data| {
            let previous_items = previous_data
                .map(|d| d.stack.as_slice())
                .unwrap_or_default();
            let (live_data, fossil_data) = classify_stack_entries(&current_entries, previous_items);
            let live = live_data
                .iter()
                .map(|e| StackEntry {
                    bookmark_name: e.name.clone(),
                    pr_url: e.url.clone(),
                    pr_number: e.number,
                    is_current: e.name == bookmark_name,
                    is_merged: e.is_merged,
                    closed_at: e.closed_at.clone(),
                })
                .collect();
            let fossils = fossil_data
                .iter()
                .map(|e| StackEntry {
                    bookmark_name: e.name.clone(),
                    pr_url: e.url.clone(),
                    pr_number: e.number,
                    // The current PR can never be a fossil — its bookmark is
                    // by definition still live since we're commenting on it.
                    is_current: false,
                    is_merged: e.is_merged,
                    closed_at: e.closed_at.clone(),
                })
                .collect();
            (live, fossils)
        })?;

        if did_update {
            updated += 1;
        }
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::forge::ForgeKind;
    use crate::forge::types::{ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequestRef, RepoInfo, ReviewSummary};
    use crate::jj::types::{Bookmark, GitRemote, LogEntry};
    use crate::jj::Jj;

    struct RecordingGitHub {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingGitHub {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("poisoned").clone()
        }
    }

    impl Forge for RecordingGitHub {
        fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
            Ok(vec![])
        }
        fn create_pr(
            &self,
            _o: &str,
            _r: &str,
            _t: &str,
            _b: &str,
            head: &str,
            base: &str,
            draft: bool,
        ) -> Result<PullRequest> {
            let label = if draft { "create_draft_pr" } else { "create_pr" };
            self.calls
                .lock().expect("poisoned")
                .push(format!("{label}:{head}:{base}"));
            Ok(PullRequest {
                number: 42,
                html_url: "https://github.com/o/r/pull/42".to_string(),
                title: "test".to_string(),
                body: None,
                base: PullRequestRef {
                    ref_name: base.to_string(),
                    label: String::new(),
                    sha: String::new(),
                },
                head: PullRequestRef {
                    ref_name: head.to_string(),
                    label: String::new(),
                    sha: String::new(),
                },
                draft,
                node_id: "PR_node123".to_string(),
                merged_at: None,
                requested_reviewers: vec![],
            })
        }
        fn update_pr_base(&self, _o: &str, _r: &str, n: u64, base: &str) -> Result<()> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("update_base:#{n}:{base}"));
            Ok(())
        }
        fn request_reviewers(
            &self,
            _o: &str,
            _r: &str,
            n: u64,
            revs: &[String],
        ) -> Result<()> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("request_reviewers:#{n}:{}", revs.join(",")));
            Ok(())
        }
        fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> {
            Ok(vec![])
        }
        fn create_comment(
            &self,
            _o: &str,
            _r: &str,
            number: u64,
            _b: &str,
        ) -> Result<IssueComment> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("create_comment:#{number}"));
            Ok(IssueComment {
                id: 100,
                body: Some("comment".to_string()),
            })
        }
        fn update_comment(&self, _o: &str, _r: &str, id: u64, _b: &str) -> Result<()> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("update_comment:{id}"));
            Ok(())
        }
        fn update_pr_body(&self, _o: &str, _r: &str, n: u64, _body: &str) -> Result<()> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("update_pr_body:#{n}"));
            Ok(())
        }
        fn mark_pr_ready(&self, _o: &str, _r: &str, number: u64) -> Result<()> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("mark_pr_ready:#{number}"));
            Ok(())
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("testuser".to_string())
        }
        fn find_merged_pr(
            &self, _o: &str, _r: &str, _h: &str,
        ) -> Result<Option<PullRequest>> {
            Ok(None)
        }
        fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
        fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
        fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
        fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
            Ok(PrState { merged: false, state: "open".to_string() })
        }
    }

    struct RecordingJj {
        pushes: Mutex<Vec<String>>,
    }

    impl RecordingJj {
        fn new() -> Self {
            Self {
                pushes: Mutex::new(Vec::new()),
            }
        }

        fn pushes(&self) -> Vec<String> {
            self.pushes.lock().expect("poisoned").clone()
        }
    }

    impl Jj for RecordingJj {
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(vec![])
        }
        fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> {
            Ok(vec![])
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
            Ok(vec![])
        }
        fn get_default_branch(&self) -> Result<String> {
            Ok("main".to_string())
        }
        fn push_bookmark(&self, name: &str, remote: &str) -> Result<()> {
            self.pushes.lock().expect("poisoned").push(format!("{name}:{remote}"));
            Ok(())
        }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok("wc_commit".to_string())
        }
        fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { unimplemented!() }
        fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { unimplemented!() }
        fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
            Ok(vec!["dummy_commit_id".to_string()])
        }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
    }

    fn make_bookmark(name: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: format!("c_{name}"),
            change_id: format!("ch_{name}"),
            has_remote: false,
            is_synced: false,
        }
    }

    fn make_plan() -> SubmissionPlan {
        SubmissionPlan {
            bookmarks_needing_push: vec![make_bookmark("auth")],
            bookmarks_needing_pr: vec![super::super::plan::BookmarkNeedingPr {
                bookmark: make_bookmark("auth"),
                base_branch: "main".to_string(),
                title: "Add auth".to_string(),
                body: "Auth body".to_string(),
            }],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo {
                owner: "o".to_string(),
                repo: "r".to_string(),
            },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        }
    }

    #[test]
    fn test_dry_run_produces_no_side_effects() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();
        let plan = make_plan();

        execute_submission_plan(&jj, &github, &plan, &[], true).unwrap();

        assert!(jj.pushes().is_empty(), "dry run should not push");
        assert!(
            github.calls().is_empty(),
            "dry run should not call GitHub API"
        );
    }

    #[test]
    fn test_creates_pr_with_correct_base() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();
        let plan = make_plan();

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert_eq!(jj.pushes(), vec!["auth:origin"]);
        assert!(github.calls().iter().any(|c| c == "create_pr:auth:main"));
    }

    #[test]
    fn test_requests_reviewers_on_new_prs() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();
        let plan = make_plan();

        let reviewers = vec!["alice".to_string(), "bob".to_string()];
        execute_submission_plan(&jj, &github, &plan, &reviewers, false).unwrap();

        assert!(github
            .calls()
            .iter()
            .any(|c| c == "request_reviewers:#42:alice,bob"));
    }

    #[test]
    fn test_no_reviewers_when_list_empty() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();
        let plan = make_plan();

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(
            !github
                .calls()
                .iter()
                .any(|c| c.starts_with("request_reviewers")),
            "should not request reviewers when list is empty"
        );
    }

    #[test]
    fn test_single_pr_skips_stack_comment() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();
        let plan = make_plan();

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(
            !github
                .calls()
                .iter()
                .any(|c| c.starts_with("create_comment")),
            "single-PR stack should not get a stack comment"
        );
    }

    #[test]
    fn test_two_prs_creates_stack_comments() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let mut plan = make_plan();
        // Add a second bookmark+PR so the stack has 2 PRs
        plan.all_bookmarks.push(make_bookmark("profile"));
        plan.bookmarks_needing_pr.push(super::super::plan::BookmarkNeedingPr {
            bookmark: make_bookmark("profile"),
            base_branch: "auth".to_string(),
            title: "Add profile".to_string(),
            body: "Profile body".to_string(),
        });

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        let comment_calls: Vec<_> = github
            .calls()
            .iter()
            .filter(|c| c.starts_with("create_comment"))
            .cloned()
            .collect();
        assert_eq!(
            comment_calls.len(),
            2,
            "two-PR stack should get comments on both PRs: {comment_calls:?}"
        );
    }

    #[test]
    fn test_updates_existing_stack_comment() {
        let jj = RecordingJj::new();

        struct GitHubWithExistingComment {
            calls: Mutex<Vec<String>>,
        }

        impl Forge for GitHubWithExistingComment {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn create_pr(
                &self, _o: &str, _r: &str, _t: &str, _b: &str,
                _h: &str, _ba: &str, _draft: bool,
            ) -> Result<PullRequest> {
                unimplemented!()
            }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
                unimplemented!()
            }
            fn request_reviewers(
                &self, _o: &str, _r: &str, _n: u64, _revs: &[String],
            ) -> Result<()> {
                unimplemented!()
            }
            fn list_comments(
                &self,
                _o: &str,
                _r: &str,
                _i: u64,
            ) -> Result<Vec<IssueComment>> {
                Ok(vec![IssueComment {
                    id: 99,
                    body: Some("<!-- jjpr:stack-info -->\nold comment".to_string()),
                }])
            }
            fn create_comment(
                &self,
                _o: &str,
                _r: &str,
                _i: u64,
                _b: &str,
            ) -> Result<IssueComment> {
                panic!("should update, not create");
            }
            fn update_comment(&self, _o: &str, _r: &str, id: u64, _b: &str) -> Result<()> {
                self.calls
                    .lock().expect("poisoned")
                    .push(format!("update_comment:{id}"));
                Ok(())
            }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
                Ok(())
            }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> {
                Ok(())
            }
            fn get_authenticated_user(&self) -> Result<String> {
                Ok("testuser".to_string())
            }
            fn find_merged_pr(
                &self, _o: &str, _r: &str, _h: &str,
            ) -> Result<Option<PullRequest>> {
                Ok(None)
            }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let github = GitHubWithExistingComment {
            calls: Mutex::new(Vec::new()),
        };

        let existing_pr = PullRequest {
            number: 10,
            html_url: "https://github.com/o/r/pull/10".to_string(),
            title: "Add auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([("auth".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo {
                owner: "o".to_string(),
                repo: "r".to_string(),
            },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        let calls = github.calls.lock().expect("poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "update_comment:99");
    }

    #[test]
    fn test_updates_pr_base() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let existing_pr = PullRequest {
            number: 5,
            html_url: "https://github.com/o/r/pull/5".to_string(),
            title: "profile".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "profile".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![super::super::plan::BookmarkNeedingBaseUpdate {
                bookmark: make_bookmark("profile"),
                pr: existing_pr.clone(),
                expected_base: "auth".to_string(),
            }],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([("profile".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo {
                owner: "o".to_string(),
                repo: "r".to_string(),
            },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("profile")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(github.calls().iter().any(|c| c == "update_base:#5:auth"));
    }

    #[test]
    fn test_execute_updates_pr_body() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![super::super::plan::BookmarkNeedingBodyUpdate {
                bookmark: make_bookmark("auth"),
                pr_number: 10,
                new_body: "Updated body".to_string(),
            }],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([(
                "auth".to_string(),
                PullRequest {
                    number: 10,
                    html_url: "https://github.com/o/r/pull/10".to_string(),
                    title: "Old title".to_string(),
                    body: None,
                    base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                    head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
                    draft: false,
                    node_id: String::new(),
                    merged_at: None,
                    requested_reviewers: vec![],
                },
            )]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(
            github.calls().iter().any(|c| c == "update_pr_body:#10"),
            "should call update_pr_body"
        );
    }

    #[test]
    fn test_dry_run_skips_body_update() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![super::super::plan::BookmarkNeedingBodyUpdate {
                bookmark: make_bookmark("auth"),
                pr_number: 10,
                new_body: "Updated body".to_string(),
            }],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], true).unwrap();

        assert!(
            !github.calls().iter().any(|c| c.starts_with("update_pr_body")),
            "dry run should not call update_pr_body"
        );
    }

    #[test]
    fn test_create_pr_as_draft() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let mut plan = make_plan();
        plan.draft = true;

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(
            github.calls().iter().any(|c| c.starts_with("create_draft_pr:")),
            "should pass draft=true to create_pr: {:?}",
            github.calls()
        );
    }

    #[test]
    fn test_ready_converts_draft_prs() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![super::super::plan::BookmarkNeedingReady {
                bookmark: make_bookmark("auth"),
                pr_number: 10,
            }],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(
            github.calls().iter().any(|c| c == "mark_pr_ready:#10"),
            "should call mark_pr_ready: {:?}",
            github.calls()
        );
    }

    #[test]
    fn test_requests_reviewers_on_existing_prs() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![(make_bookmark("auth"), 10)],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        let reviewers = vec!["alice".to_string()];
        execute_submission_plan(&jj, &github, &plan, &reviewers, false).unwrap();

        assert!(
            github.calls().iter().any(|c| c == "request_reviewers:#10:alice"),
            "should request reviewers on existing PRs: {:?}",
            github.calls()
        );
    }

    #[test]
    fn test_skips_already_requested_reviewers() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let existing_pr = PullRequest {
            number: 10,
            html_url: "https://github.com/o/r/pull/10".to_string(),
            title: "Add auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec!["alice".to_string()],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![(make_bookmark("auth"), 10)],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([("auth".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        let reviewers = vec!["alice".to_string(), "bob".to_string()];
        execute_submission_plan(&jj, &github, &plan, &reviewers, false).unwrap();

        assert!(
            github.calls().iter().any(|c| c == "request_reviewers:#10:alice,bob"),
            "should pass full reviewer set (existing + new) to forge: {:?}",
            github.calls()
        );
    }

    #[test]
    fn test_skips_reviewer_request_when_all_already_requested() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let existing_pr = PullRequest {
            number: 10,
            html_url: "https://github.com/o/r/pull/10".to_string(),
            title: "Add auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec!["alice".to_string(), "bob".to_string()],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![(make_bookmark("auth"), 10)],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([("auth".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        let reviewers = vec!["alice".to_string(), "bob".to_string()];
        execute_submission_plan(&jj, &github, &plan, &reviewers, false).unwrap();

        assert!(
            !github.calls().iter().any(|c| c.starts_with("request_reviewers")),
            "should not request reviewers when all already requested: {:?}",
            github.calls()
        );
    }

    #[test]
    fn test_skips_reviewer_case_insensitive() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let existing_pr = PullRequest {
            number: 10,
            html_url: "https://github.com/o/r/pull/10".to_string(),
            title: "Add auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec!["Alice".to_string()],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![(make_bookmark("auth"), 10)],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([("auth".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        let reviewers = vec!["alice".to_string()];
        execute_submission_plan(&jj, &github, &plan, &reviewers, false).unwrap();

        assert!(
            !github.calls().iter().any(|c| c.starts_with("request_reviewers")),
            "should match reviewers case-insensitively: {:?}",
            github.calls()
        );
    }

    #[test]
    fn test_partial_failure_reports_completed_actions() {
        struct FailingJj;
        impl Jj for FailingJj {
            fn git_fetch(&self) -> Result<()> { Ok(()) }
            fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
            fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> { Ok(vec![]) }
            fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
            fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
            fn push_bookmark(&self, name: &str, _remote: &str) -> Result<()> {
                if name == "profile" {
                    anyhow::bail!("push failed for profile")
                }
                Ok(())
            }
            fn get_working_copy_commit_id(&self) -> Result<String> { Ok("wc".to_string()) }
            fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { unimplemented!() }
            fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { unimplemented!() }
            fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
                Ok(vec!["dummy_commit_id".to_string()])
            }
            fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
        }

        let github = RecordingGitHub::new();

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![make_bookmark("auth"), make_bookmark("profile")],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth"), make_bookmark("profile")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        let err = execute_submission_plan(&FailingJj, &github, &plan, &[], false).unwrap_err();
        assert!(err.to_string().contains("push failed for profile"));
    }

    #[test]
    fn test_dry_run_skips_reviewer_requests_on_existing() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![(make_bookmark("auth"), 10)],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        let reviewers = vec!["alice".to_string()];
        execute_submission_plan(&jj, &github, &plan, &reviewers, true).unwrap();

        assert!(
            github.calls().is_empty(),
            "dry run should not call any GitHub API: {:?}",
            github.calls()
        );
    }

    #[test]
    fn test_noop_plan_succeeds_without_api_calls() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(jj.pushes().is_empty());
        assert!(github.calls().is_empty());
    }

    #[test]
    fn test_has_actions_empty_plan() {
        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        assert!(!plan.has_actions());
    }

    #[test]
    fn test_has_actions_with_push() {
        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![make_bookmark("auth")],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };
        assert!(plan.has_actions());
    }

    #[test]
    fn test_stack_comment_excludes_default_branch() {
        let jj = RecordingJj::new();

        struct CapturingGitHub {
            calls: Mutex<Vec<String>>,
            comment_bodies: Mutex<Vec<String>>,
        }

        impl Forge for CapturingGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> { Ok(vec![]) }
            fn create_pr(
                &self, _o: &str, _r: &str, _t: &str, _b: &str,
                _h: &str, _ba: &str, _draft: bool,
            ) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
            fn create_comment(&self, _o: &str, _r: &str, number: u64, body: &str) -> Result<IssueComment> {
                self.calls.lock().expect("poisoned").push(format!("create_comment:#{number}"));
                self.comment_bodies.lock().expect("poisoned").push(body.to_string());
                Ok(IssueComment { id: 100, body: Some(body.to_string()) })
            }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { Ok(()) }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { Ok(()) }
            fn get_authenticated_user(&self) -> Result<String> { Ok("testuser".to_string()) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { Ok(None) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let github = CapturingGitHub {
            calls: Mutex::new(Vec::new()),
            comment_bodies: Mutex::new(Vec::new()),
        };

        let auth_pr = PullRequest {
            number: 1,
            html_url: "https://github.com/o/r/pull/1".to_string(),
            title: "auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        };

        let profile_pr = PullRequest {
            number: 2,
            html_url: "https://github.com/o/r/pull/2".to_string(),
            title: "profile".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "profile".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([
                ("auth".to_string(), auth_pr),
                ("profile".to_string(), profile_pr),
            ]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            // main is in all_bookmarks (the bug scenario) along with auth and profile
            all_bookmarks: vec![make_bookmark("main"), make_bookmark("auth"), make_bookmark("profile")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        // Should create comments for "auth" and "profile", not for "main"
        let calls = github.calls.lock().expect("poisoned");
        assert_eq!(calls.len(), 2, "should create exactly two comments: {calls:?}");
        assert!(calls.contains(&"create_comment:#1".to_string()));
        assert!(calls.contains(&"create_comment:#2".to_string()));

        // The comment bodies should not mention "main"
        let bodies = github.comment_bodies.lock().expect("poisoned");
        for body in bodies.iter() {
            assert!(!body.contains("`main`"), "comment should not contain main: {body}");
        }
    }

    #[test]
    fn test_merged_pr_links_preserved_in_stack_comments() {
        let jj = RecordingJj::new();

        struct CapturingGitHub {
            comment_bodies: Mutex<Vec<String>>,
        }

        impl Forge for CapturingGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> { Ok(vec![]) }
            fn create_pr(
                &self, _o: &str, _r: &str, _t: &str, _b: &str,
                _h: &str, _ba: &str, _draft: bool,
            ) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _revs: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { Ok(vec![]) }
            fn create_comment(&self, _o: &str, _r: &str, _number: u64, body: &str) -> Result<IssueComment> {
                self.comment_bodies.lock().expect("poisoned").push(body.to_string());
                Ok(IssueComment { id: 100, body: Some(body.to_string()) })
            }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { Ok(()) }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { Ok(()) }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { Ok(()) }
            fn get_authenticated_user(&self) -> Result<String> { Ok("testuser".to_string()) }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { Ok(None) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let github = CapturingGitHub {
            comment_bodies: Mutex::new(Vec::new()),
        };

        // "auth" is merged, "profile" is still open
        let profile_pr = PullRequest {
            number: 2,
            html_url: "https://github.com/o/r/pull/2".to_string(),
            title: "profile".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "profile".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![super::super::plan::MergedBookmark {
                bookmark: make_bookmark("auth"),
                pr_number: 1,
                html_url: "https://github.com/o/r/pull/1".to_string(),
                merged_at: Some("2026-01-01T00:00:00Z".to_string()),
            }],
            existing_prs: HashMap::from([("profile".to_string(), profile_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth"), make_bookmark("profile")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        let bodies = github.comment_bodies.lock().expect("poisoned");
        assert_eq!(bodies.len(), 1, "should create comment on profile PR");
        // The comment on profile should still link to the merged auth PR
        assert!(
            bodies[0].contains("pull/1"),
            "comment should contain link to merged auth PR #1: {}",
            bodies[0]
        );
        assert!(
            bodies[0].contains("`auth`"),
            "comment should mention auth bookmark: {}",
            bodies[0]
        );
    }

    #[test]
    fn test_title_drift_escapes_single_quotes() {
        let title = "Fix the user's login";
        let escaped = title.replace('\'', "'\\''");
        assert_eq!(escaped, "Fix the user'\\''s login");
    }

    #[test]
    fn test_title_drift_shell_metacharacters() {
        // Single quotes neutralize all shell metacharacters
        let title = "Fix $(echo pwned) `rm -rf` $HOME";
        let escaped = title.replace('\'', "'\\''");
        // No single quotes in input, so it passes through unchanged
        assert_eq!(escaped, title);
        // When wrapped in single quotes, shell will not interpret the metacharacters
        let hint = format!("gh pr edit 42 --title '{escaped}'");
        assert!(hint.contains("'Fix $(echo pwned) `rm -rf` $HOME'"));
    }

    #[test]
    fn test_comment_failure_does_not_abort() {
        let jj = RecordingJj::new();

        struct CommentFailsGitHub;
        impl Forge for CommentFailsGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn create_pr(
                &self, _o: &str, _r: &str, _t: &str, _b: &str,
                _h: &str, _ba: &str, _draft: bool,
            ) -> Result<PullRequest> {
                unimplemented!()
            }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
                unimplemented!()
            }
            fn request_reviewers(
                &self, _o: &str, _r: &str, _n: u64, _revs: &[String],
            ) -> Result<()> {
                unimplemented!()
            }
            fn list_comments(
                &self, _o: &str, _r: &str, _i: u64,
            ) -> Result<Vec<IssueComment>> {
                anyhow::bail!("GitHub API rate limited")
            }
            fn create_comment(
                &self, _o: &str, _r: &str, _i: u64, _b: &str,
            ) -> Result<IssueComment> {
                unimplemented!()
            }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> {
                unimplemented!()
            }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
                Ok(())
            }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> {
                Ok(())
            }
            fn get_authenticated_user(&self) -> Result<String> {
                Ok("testuser".to_string())
            }
            fn find_merged_pr(
                &self, _o: &str, _r: &str, _h: &str,
            ) -> Result<Option<PullRequest>> {
                Ok(None)
            }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let existing_pr = PullRequest {
            number: 10,
            html_url: "https://github.com/o/r/pull/10".to_string(),
            title: "Add auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([("auth".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo {
                owner: "o".to_string(),
                repo: "r".to_string(),
            },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
            stack_nav: crate::config::StackNavMode::Comment,
        };

        // Comment creation fails, but submission should still succeed
        let result = execute_submission_plan(&jj, &CommentFailsGitHub, &plan, &[], false);
        assert!(result.is_ok(), "comment failure should not abort: {result:?}");
    }

    // ----- classify_stack_entries unit tests -----
    //
    // Every test below uses `classify_stack_entries(current, previous)` and
    // unpacks the returned `(live, fossils)`. Live entries follow the
    // current local graph order (base→top); fossils are sorted by
    // `closed_at` descending (most recent first), with timestampless
    // entries trailing in stable insertion order.

    fn live(name: &str, num: u64) -> EntryData {
        EntryData {
            name: name.into(),
            url: Some(format!("url_{name}")),
            number: Some(num),
            is_merged: false,
            closed_at: None,
        }
    }

    fn fossil(name: &str, num: u64, closed_at: &str) -> EntryData {
        EntryData {
            name: name.into(),
            url: Some(format!("url_{name}")),
            number: Some(num),
            is_merged: true,
            closed_at: Some(closed_at.into()),
        }
    }

    fn prev_item(name: &str, num: u64, is_merged: bool) -> comment::StackCommentItem {
        comment::StackCommentItem {
            bookmark_name: name.into(),
            pr_url: format!("url_{name}"),
            pr_number: num,
            is_merged,
            closed_at: None,
        }
    }

    fn prev_fossil(name: &str, num: u64, closed_at: &str) -> comment::StackCommentItem {
        comment::StackCommentItem {
            bookmark_name: name.into(),
            pr_url: format!("url_{name}"),
            pr_number: num,
            is_merged: true,
            closed_at: Some(closed_at.into()),
        }
    }

    fn names(entries: &[EntryData]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    // -- Live ordering --

    /// New bottom-of-stack PR (real-world beancounter #1875 scenario):
    /// must render at position 0, not appended.
    #[test]
    fn test_classify_inserts_new_bottom_at_position_zero() {
        let current = vec![live("X", 1875), live("A", 1864), live("B", 1862)];
        let previous = vec![prev_item("A", 1864, false), prev_item("B", 1862, false)];

        let (live_out, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(names(&live_out), vec!["X", "A", "B"]);
        assert!(fossils_out.is_empty());
    }

    /// New PR inserted in the middle of the stack appears in its current
    /// position, not appended.
    #[test]
    fn test_classify_inserts_new_middle_in_correct_position() {
        let current = vec![live("A", 1), live("X", 2), live("B", 3)];
        let previous = vec![prev_item("A", 1, false), prev_item("B", 3, false)];

        let (live_out, _) = classify_stack_entries(&current, &previous);
        assert_eq!(names(&live_out), vec!["A", "X", "B"]);
    }

    /// When the order of shared bookmarks differs between previous and
    /// current (e.g., after a base swap), current order must win.
    #[test]
    fn test_classify_uses_current_order_when_previous_disagrees() {
        let current = vec![live("A", 1), live("B", 2), live("C", 3)];
        let previous = vec![
            prev_item("B", 2, false),
            prev_item("A", 1, false),
            prev_item("C", 3, false),
        ];

        let (live_out, _) = classify_stack_entries(&current, &previous);
        assert_eq!(names(&live_out), vec!["A", "B", "C"]);
    }

    /// Beancounter PR #1864 bug — every PR's comment must agree on the
    /// canonical base→top order regardless of its individual previous data.
    #[test]
    fn test_classify_real_world_pr_1864_scenario() {
        let current = vec![
            live("X", 1875),
            live("A", 1864),
            live("B", 1862),
            live("C", 1863),
        ];
        // PR 1864's stale previous comment had A as the base.
        let previous_for_1864 = vec![
            prev_item("A", 1864, false),
            prev_item("B", 1862, false),
            prev_item("C", 1863, false),
        ];
        let (live_1864, fossils_1864) = classify_stack_entries(&current, &previous_for_1864);
        assert_eq!(names(&live_1864), vec!["X", "A", "B", "C"]);
        assert!(fossils_1864.is_empty());

        // PR 1862's stale previous comment had stale 1861 at the base.
        let previous_for_1862 = vec![
            prev_fossil("M", 1861, "2026-04-01T00:00:00Z"),
            prev_item("A", 1864, false),
            prev_item("B", 1862, false),
            prev_item("C", 1863, false),
        ];
        let (live_1862, fossils_1862) = classify_stack_entries(&current, &previous_for_1862);
        assert_eq!(names(&live_1862), vec!["X", "A", "B", "C"]);
        assert_eq!(fossils_1862.len(), 1);
        assert_eq!(fossils_1862[0].name, "M");
    }

    /// New base displaces a merged predecessor: X (live, new base) must
    /// come before A (live, was the previous base); M (merged) goes to
    /// fossils, not the live list.
    #[test]
    fn test_classify_new_base_displaces_merged_predecessor() {
        let current = vec![live("X", 1875), live("A", 1862), live("B", 1863)];
        let previous = vec![
            prev_fossil("M", 1861, "2026-04-01T00:00:00Z"),
            prev_item("A", 1862, false),
            prev_item("B", 1863, false),
        ];

        let (live_out, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(
            names(&live_out),
            vec!["X", "A", "B"],
            "live list is just the live local stack"
        );
        assert_eq!(names(&fossils_out), vec!["M"], "M moves to fossils");
    }

    /// A bookmark whose URL/number changed between submits reflects the
    /// *current* metadata, not the stale previous one.
    #[test]
    fn test_classify_prefers_current_metadata_for_shared_bookmarks() {
        let current = vec![EntryData {
            name: "A".into(),
            url: Some("new_url_a".into()),
            number: Some(99),
            is_merged: false,
            closed_at: None,
        }];
        let previous = vec![comment::StackCommentItem {
            bookmark_name: "A".into(),
            pr_url: "old_url_a".into(),
            pr_number: 1,
            is_merged: false,
            closed_at: None,
        }];

        let (live_out, _) = classify_stack_entries(&current, &previous);
        assert_eq!(live_out.len(), 1);
        assert_eq!(live_out[0].url.as_deref(), Some("new_url_a"));
        assert_eq!(live_out[0].number, Some(99));
    }

    #[test]
    fn test_classify_empty_inputs() {
        let (live_out, fossils_out) = classify_stack_entries(&[], &[]);
        assert!(live_out.is_empty());
        assert!(fossils_out.is_empty());
    }

    #[test]
    fn test_classify_empty_previous() {
        let current = vec![live("A", 1), live("B", 2)];
        let (live_out, fossils_out) = classify_stack_entries(&current, &[]);
        assert_eq!(names(&live_out), vec!["A", "B"]);
        assert!(fossils_out.is_empty());
    }

    // -- Fossil ordering --

    /// Fossils sort by closed_at descending (most recent first).
    #[test]
    fn test_classify_fossils_sorted_by_recency() {
        let current = vec![live("live_one", 100)];
        let previous = vec![
            prev_fossil("oldest", 1, "2026-01-01T00:00:00Z"),
            prev_fossil("newest", 3, "2026-03-01T00:00:00Z"),
            prev_fossil("middle", 2, "2026-02-01T00:00:00Z"),
        ];
        let (_, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(names(&fossils_out), vec!["newest", "middle", "oldest"]);
    }

    /// Fossils with no timestamp sort to the end; among them, insertion
    /// order is preserved (stable sort).
    #[test]
    fn test_classify_timestampless_fossils_sort_to_end() {
        let current = vec![live("top", 100)];
        let previous = vec![
            comment::StackCommentItem {
                bookmark_name: "no_ts_first".into(),
                pr_url: "u1".into(),
                pr_number: 1,
                is_merged: true,
                closed_at: None,
            },
            prev_fossil("with_ts", 2, "2026-03-01T00:00:00Z"),
            comment::StackCommentItem {
                bookmark_name: "no_ts_second".into(),
                pr_url: "u3".into(),
                pr_number: 3,
                is_merged: true,
                closed_at: None,
            },
        ];
        let (_, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(
            names(&fossils_out),
            vec!["with_ts", "no_ts_first", "no_ts_second"],
            "timestamped fossils first; then timestampless in insertion order"
        );
    }

    /// A current entry marked is_merged with no closed_at backfills its
    /// timestamp from the previous comment if available.
    #[test]
    fn test_classify_backfills_closed_at_from_previous() {
        // Current sees A as merged but lacks the timestamp (e.g., transient
        // forge error during this submit).
        let current = vec![EntryData {
            name: "A".into(),
            url: Some("url_a".into()),
            number: Some(1),
            is_merged: true,
            closed_at: None,
        }];
        let previous = vec![prev_fossil("A", 1, "2026-04-01T00:00:00Z")];

        let (_, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(fossils_out.len(), 1);
        assert_eq!(
            fossils_out[0].closed_at.as_deref(),
            Some("2026-04-01T00:00:00Z"),
            "closed_at must inherit from previous when current is missing it"
        );
    }

    /// Current's closed_at takes precedence over previous's when both have
    /// values (current is fresher from the forge).
    #[test]
    fn test_classify_current_closed_at_takes_precedence() {
        let current = vec![fossil("A", 1, "2026-05-01T00:00:00Z")];
        let previous = vec![prev_fossil("A", 1, "2026-04-01T00:00:00Z")];
        let (_, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(fossils_out.len(), 1);
        assert_eq!(
            fossils_out[0].closed_at.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
    }

    /// Previous-only fossils (bookmark cleaned up locally) inherit their
    /// closed_at from JJPR_DATA — the durable shared store.
    #[test]
    fn test_classify_previous_only_fossil_keeps_closed_at() {
        let current: Vec<EntryData> = vec![live("top", 100)];
        let previous = vec![prev_fossil("gone", 1, "2026-04-30T12:00:00Z")];

        let (_, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(fossils_out.len(), 1);
        assert_eq!(fossils_out[0].name, "gone");
        assert_eq!(
            fossils_out[0].closed_at.as_deref(),
            Some("2026-04-30T12:00:00Z")
        );
    }

    /// All known fossils (current and previous-only) appear in the
    /// returned list — the cap is enforced at render time, not classify
    /// time, so that JJPR_DATA preserves the full history.
    #[test]
    fn test_classify_does_not_truncate_fossils() {
        let current = vec![live("top", 100)];
        let previous: Vec<comment::StackCommentItem> = (1..=12)
            .map(|i| prev_fossil(&format!("old{i}"), i, &format!("2026-01-{:02}T00:00:00Z", i)))
            .collect();
        let (_, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(
            fossils_out.len(),
            12,
            "classify keeps every fossil; the renderer caps display"
        );
    }

    /// Beancounter PR #1862's stale comment listed a merged predecessor
    /// (#1861) and a new base (#1875). Live list must follow current;
    /// fossil list contains M only.
    #[test]
    fn test_classify_beancounter_1862_full_scenario() {
        let current = vec![
            live("X", 1875),
            live("A", 1864),
            live("B", 1862),
            live("C", 1863),
        ];
        let previous = vec![
            prev_fossil("M", 1861, "2026-04-01T00:00:00Z"),
            prev_item("A", 1864, false),
            prev_item("B", 1862, false),
            prev_item("C", 1863, false),
        ];
        let (live_out, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(names(&live_out), vec!["X", "A", "B", "C"]);
        assert_eq!(names(&fossils_out), vec!["M"]);
    }

    /// Mix of current-merged, previous-only-merged, and live entries.
    #[test]
    fn test_classify_mixed_live_and_fossil() {
        let current = vec![
            live("live1", 1),
            fossil("recent_merge", 2, "2026-04-15T00:00:00Z"),
            live("live2", 3),
        ];
        let previous = vec![
            prev_fossil("ancient_gone", 4, "2026-01-01T00:00:00Z"),
            prev_item("live1", 1, false),
            prev_fossil("recent_merge", 2, "2026-04-15T00:00:00Z"),
            prev_item("live2", 3, false),
        ];
        let (live_out, fossils_out) = classify_stack_entries(&current, &previous);
        assert_eq!(names(&live_out), vec!["live1", "live2"]);
        assert_eq!(
            names(&fossils_out),
            vec!["recent_merge", "ancient_gone"],
            "fossils sorted recent-first regardless of source"
        );
    }
}
