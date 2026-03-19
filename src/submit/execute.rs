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
            "  Skipping '{}' — {} already merged",
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

        // Show PR URL if this bookmark has an existing PR
        if let Some(pr) = plan.existing_prs.get(&bookmark.name) {
            println!("    {}", pr.html_url);
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

    // Phase 7: Update/create stack comments on all PRs
    let comments_updated = if dry_run {
        println!("  Would update stack comments");
        0
    } else {
        match update_stack_comments(github, plan, &bookmark_to_pr) {
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
struct EntryData {
    name: String,
    url: Option<String>,
    number: Option<u64>,
    is_merged: bool,
}

/// Merge current entries with previous comment data so links are never lost.
///
/// Strategy: iterate previous items in order, replacing with current data when
/// the bookmark still exists. Items not in current are preserved as merged.
/// New current items not in previous are appended at the end.
fn merge_with_previous_entries(
    current: &[EntryData],
    previous: &[comment::StackCommentItem],
) -> Vec<EntryData> {
    use std::collections::HashSet;

    let current_by_name: HashMap<&str, &EntryData> = current
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    let mut seen: HashSet<&str> = HashSet::new();
    let mut result = Vec::new();

    // Previous items in their original order
    for prev in previous {
        seen.insert(&prev.bookmark_name);
        if let Some(cur) = current_by_name.get(prev.bookmark_name.as_str()) {
            result.push(EntryData {
                name: cur.name.clone(),
                url: cur.url.clone(),
                number: cur.number,
                is_merged: cur.is_merged,
            });
        } else {
            // Not in current segments — preserve as merged
            result.push(EntryData {
                name: prev.bookmark_name.clone(),
                url: Some(prev.pr_url.clone()),
                number: Some(prev.pr_number),
                is_merged: true,
            });
        }
    }

    // Append new entries not in previous
    for cur in current {
        if !seen.contains(cur.name.as_str()) {
            result.push(EntryData {
                name: cur.name.clone(),
                url: cur.url.clone(),
                number: cur.number,
                is_merged: cur.is_merged,
            });
        }
    }

    result
}

fn update_stack_comments(
    github: &dyn Forge,
    plan: &SubmissionPlan,
    bookmark_to_pr: &HashMap<String, PullRequest>,
) -> Result<usize> {
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;
    let mut updated = 0;

    // Build a lookup for merged PRs so their links are preserved in comments
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
                }
            } else if let Some(merged) = merged_prs.get(b.name.as_str()) {
                EntryData {
                    name: b.name.clone(),
                    url: Some(merged.html_url.clone()),
                    number: Some(merged.pr_number),
                    is_merged: true,
                }
            } else {
                EntryData {
                    name: b.name.clone(),
                    url: None,
                    number: None,
                    is_merged: false,
                }
            }
        })
        .collect();

    for bookmark in plan.all_bookmarks.iter().filter(|b| b.name != plan.default_branch) {
        let Some(pr) = bookmark_to_pr.get(&bookmark.name) else {
            continue;
        };

        // Fetch existing comment first so we can merge previous data
        let comments = github.list_comments(owner, repo, pr.number)?;
        let existing = comment::find_stack_comment(&comments);

        let previous_items: Vec<comment::StackCommentItem> = existing
            .and_then(|c| c.body.as_deref())
            .and_then(comment::parse_comment_data)
            .map(|d| d.stack)
            .unwrap_or_default();

        let merged = merge_with_previous_entries(&current_entries, &previous_items);

        let entries: Vec<StackEntry> = merged
            .iter()
            .map(|e| StackEntry {
                bookmark_name: e.name.clone(),
                pr_url: e.url.clone(),
                pr_number: e.number,
                is_current: e.name == bookmark.name,
                is_merged: e.is_merged,
            })
            .collect();

        let body = comment::generate_comment_body(&entries);

        if let Some(existing_comment) = existing {
            if existing_comment.body.as_deref() != Some(&body) {
                github.update_comment(owner, repo, existing_comment.id, &body)?;
                updated += 1;
            }
        } else {
            github.create_comment(owner, repo, pr.number, &body)?;
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
    fn test_creates_stack_comments() {
        let jj = RecordingJj::new();
        let github = RecordingGitHub::new();
        let plan = make_plan();

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(
            github
                .calls()
                .iter()
                .any(|c| c.starts_with("create_comment")),
            "should create stack comments on PRs"
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

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            bookmarks_needing_body_update: vec![],
            bookmarks_needing_ready: vec![],
            bookmarks_needing_reviewers: vec![],
            bookmarks_with_title_drift: vec![],
            bookmarks_already_merged: vec![],
            existing_prs: HashMap::from([("auth".to_string(), auth_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            // main is in all_bookmarks (the bug scenario)
            all_bookmarks: vec![make_bookmark("main"), make_bookmark("auth")],
            default_branch: "main".to_string(),
            draft: false,
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        // Should only create a comment for "auth", not for "main"
        let calls = github.calls.lock().expect("poisoned");
        assert_eq!(calls.len(), 1, "should create exactly one comment: {calls:?}");
        assert_eq!(calls[0], "create_comment:#1");

        // The comment body should not mention "main"
        let bodies = github.comment_bodies.lock().expect("poisoned");
        assert!(!bodies[0].contains("`main`"), "comment should not contain main: {}", bodies[0]);
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
            }],
            existing_prs: HashMap::from([("profile".to_string(), profile_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo { owner: "o".to_string(), repo: "r".to_string() },
            forge_kind: ForgeKind::GitHub,
            all_bookmarks: vec![make_bookmark("auth"), make_bookmark("profile")],
            default_branch: "main".to_string(),
            draft: false,
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
        };

        // Comment creation fails, but submission should still succeed
        let result = execute_submission_plan(&jj, &CommentFailsGitHub, &plan, &[], false);
        assert!(result.is_ok(), "comment failure should not abort: {result:?}");
    }

    #[test]
    fn test_merge_previous_entries_preserves_removed() {
        // Previous: [A, B, C], Current: [B, C] → [A(merged), B, C]
        let current = vec![
            EntryData { name: "B".into(), url: Some("url_b".into()), number: Some(2), is_merged: false },
            EntryData { name: "C".into(), url: Some("url_c".into()), number: Some(3), is_merged: false },
        ];
        let previous = vec![
            comment::StackCommentItem { bookmark_name: "A".into(), pr_url: "url_a".into(), pr_number: 1, is_merged: false },
            comment::StackCommentItem { bookmark_name: "B".into(), pr_url: "url_b".into(), pr_number: 2, is_merged: false },
            comment::StackCommentItem { bookmark_name: "C".into(), pr_url: "url_c".into(), pr_number: 3, is_merged: false },
        ];

        let result = merge_with_previous_entries(&current, &previous);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "A");
        assert!(result[0].is_merged, "A should be marked merged");
        assert_eq!(result[1].name, "B");
        assert!(!result[1].is_merged);
        assert_eq!(result[2].name, "C");
        assert!(!result[2].is_merged);
    }

    #[test]
    fn test_merge_previous_entries_appends_new() {
        // Previous: [A, B], Current: [B, C] → [A(merged), B, C]
        let current = vec![
            EntryData { name: "B".into(), url: Some("url_b".into()), number: Some(2), is_merged: false },
            EntryData { name: "C".into(), url: Some("url_c".into()), number: Some(3), is_merged: false },
        ];
        let previous = vec![
            comment::StackCommentItem { bookmark_name: "A".into(), pr_url: "url_a".into(), pr_number: 1, is_merged: false },
            comment::StackCommentItem { bookmark_name: "B".into(), pr_url: "url_b".into(), pr_number: 2, is_merged: false },
        ];

        let result = merge_with_previous_entries(&current, &previous);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "A");
        assert!(result[0].is_merged);
        assert_eq!(result[1].name, "B");
        assert!(!result[1].is_merged);
        assert_eq!(result[2].name, "C");
        assert!(!result[2].is_merged);
    }

    #[test]
    fn test_merge_previous_entries_empty_previous() {
        // Previous: [], Current: [A, B] → [A, B]
        let current = vec![
            EntryData { name: "A".into(), url: Some("url_a".into()), number: Some(1), is_merged: false },
            EntryData { name: "B".into(), url: Some("url_b".into()), number: Some(2), is_merged: false },
        ];

        let result = merge_with_previous_entries(&current, &[]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "A");
        assert_eq!(result[1].name, "B");
    }

    #[test]
    fn test_merge_previous_entries_current_takes_precedence() {
        // Current has updated URL for B
        let current = vec![
            EntryData { name: "B".into(), url: Some("new_url_b".into()), number: Some(22), is_merged: false },
        ];
        let previous = vec![
            comment::StackCommentItem { bookmark_name: "A".into(), pr_url: "url_a".into(), pr_number: 1, is_merged: false },
            comment::StackCommentItem { bookmark_name: "B".into(), pr_url: "old_url_b".into(), pr_number: 2, is_merged: false },
        ];

        let result = merge_with_previous_entries(&current, &previous);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "A");
        assert!(result[0].is_merged);
        assert_eq!(result[1].name, "B");
        assert_eq!(result[1].url.as_deref(), Some("new_url_b"));
        assert_eq!(result[1].number, Some(22));
        assert!(!result[1].is_merged);
    }

    #[test]
    fn test_merge_previous_entries_no_change() {
        // Previous: [A, B], Current: [A, B] → [A, B] (unchanged)
        let current = vec![
            EntryData { name: "A".into(), url: Some("url_a".into()), number: Some(1), is_merged: false },
            EntryData { name: "B".into(), url: Some("url_b".into()), number: Some(2), is_merged: false },
        ];
        let previous = vec![
            comment::StackCommentItem { bookmark_name: "A".into(), pr_url: "url_a".into(), pr_number: 1, is_merged: false },
            comment::StackCommentItem { bookmark_name: "B".into(), pr_url: "url_b".into(), pr_number: 2, is_merged: false },
        ];

        let result = merge_with_previous_entries(&current, &previous);
        assert_eq!(result.len(), 2);
        assert!(!result[0].is_merged);
        assert!(!result[1].is_merged);
    }
}
