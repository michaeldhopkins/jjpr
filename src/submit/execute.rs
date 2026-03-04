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

    // Phase 6: Request reviewers on existing PRs
    for (bookmark, pr_number) in &plan.bookmarks_needing_reviewers {
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
        if let Err(e) = github.request_reviewers(owner, repo, *pr_number, reviewers) {
            report_partial_failure(&completed_actions);
            return Err(e);
        }
        completed_actions.push(format!("Requested reviewers on {}", fk.format_ref(*pr_number)));
    }

    // Phase 7: Update/create stack comments on all PRs
    if !dry_run
        && let Err(e) = update_stack_comments(github, plan, &bookmark_to_pr)
    {
        eprintln!("  Warning: failed to update stack comments: {e}");
        eprintln!("  (run `jjpr submit` again to retry)");
    }

    // Report title drift
    print_title_drift_warnings(&plan.bookmarks_with_title_drift, &plan.repo_info, fk);

    if !plan.has_actions() && plan.bookmarks_already_merged.is_empty() {
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
fn update_stack_comments(
    github: &dyn Forge,
    plan: &SubmissionPlan,
    bookmark_to_pr: &HashMap<String, PullRequest>,
) -> Result<()> {
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;

    // Build the stack entries list (same for every PR, just with different "is_current")
    let entries_base: Vec<(String, Option<String>, Option<u64>)> = plan
        .all_bookmarks
        .iter()
        .map(|b| {
            let pr = bookmark_to_pr.get(&b.name);
            (
                b.name.clone(),
                pr.map(|p| p.html_url.clone()),
                pr.map(|p| p.number),
            )
        })
        .collect();

    for bookmark in &plan.all_bookmarks {
        let Some(pr) = bookmark_to_pr.get(&bookmark.name) else {
            continue;
        };

        let entries: Vec<StackEntry> = entries_base
            .iter()
            .map(|(name, url, number)| StackEntry {
                bookmark_name: name.clone(),
                pr_url: url.clone(),
                pr_number: *number,
                is_current: name == &bookmark.name,
            })
            .collect();

        let body = comment::generate_comment_body(&entries);

        // Find existing stack comment
        let comments = github.list_comments(owner, repo, pr.number)?;
        let existing = comment::find_stack_comment(&comments);

        if let Some(existing_comment) = existing {
            github.update_comment(owner, repo, existing_comment.id, &body)?;
        } else {
            github.create_comment(owner, repo, pr.number, &body)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::forge::ForgeKind;
    use crate::forge::types::{ChecksStatus, IssueComment, MergeMethod, PrMergeability, PullRequestRef, RepoInfo, ReviewSummary};
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
                },
                head: PullRequestRef {
                    ref_name: head.to_string(),
                    label: String::new(),
                },
                draft,
                node_id: "PR_node123".to_string(),
                merged_at: None,
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
        }

        let github = GitHubWithExistingComment {
            calls: Mutex::new(Vec::new()),
        };

        let existing_pr = PullRequest {
            number: 10,
            html_url: "https://github.com/o/r/pull/10".to_string(),
            title: "Add auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
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
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new() },
            head: PullRequestRef { ref_name: "profile".to_string(), label: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
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
                    base: PullRequestRef { ref_name: "main".to_string(), label: String::new() },
                    head: PullRequestRef { ref_name: "auth".to_string(), label: String::new() },
                    draft: false,
                    node_id: String::new(),
                    merged_at: None,
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
        }

        let existing_pr = PullRequest {
            number: 10,
            html_url: "https://github.com/o/r/pull/10".to_string(),
            title: "Add auth".to_string(),
            body: None,
            base: PullRequestRef { ref_name: "main".to_string(), label: String::new() },
            head: PullRequestRef { ref_name: "auth".to_string(), label: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
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
}
