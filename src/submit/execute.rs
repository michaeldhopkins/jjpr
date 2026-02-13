use std::collections::HashMap;

use anyhow::Result;

use crate::github::comment::{self, StackEntry};
use crate::github::types::PullRequest;
use crate::github::GitHub;
use crate::jj::Jj;

use super::plan::SubmissionPlan;

/// Execute the submission plan: push, create PRs, update bases, manage comments.
pub fn execute_submission_plan(
    jj: &dyn Jj,
    github: &dyn GitHub,
    plan: &SubmissionPlan,
    reviewers: &[String],
    dry_run: bool,
) -> Result<()> {
    let owner = &plan.repo_info.owner;
    let repo = &plan.repo_info.repo;

    // Phase 1: Push bookmarks
    for bookmark in &plan.bookmarks_needing_push {
        if dry_run {
            println!("  Would push bookmark '{}' to {}", bookmark.name, plan.remote_name);
            continue;
        }
        println!("  Pushing '{}'...", bookmark.name);
        jj.push_bookmark(&bookmark.name, &plan.remote_name)?;
    }

    // Phase 2: Create new PRs
    let mut bookmark_to_pr: HashMap<String, PullRequest> = plan.existing_prs.clone();

    for item in &plan.bookmarks_needing_pr {
        if dry_run {
            println!(
                "  Would create PR for '{}' (base: {})",
                item.bookmark.name, item.base_branch
            );
            continue;
        }
        println!("  Creating PR for '{}'...", item.bookmark.name);
        let pr = github.create_pr(
            owner,
            repo,
            &item.title,
            &item.body,
            &item.bookmark.name,
            &item.base_branch,
        )?;
        println!("    {}", pr.html_url);

        // Request reviewers on new PRs
        if !reviewers.is_empty() {
            github.request_reviewers(owner, repo, pr.number, reviewers)?;
        }

        bookmark_to_pr.insert(item.bookmark.name.clone(), pr);
    }

    // Phase 3: Update PR bases
    for item in &plan.bookmarks_needing_base_update {
        if dry_run {
            println!(
                "  Would update PR #{} base: {} -> {}",
                item.pr.number, item.pr.base.ref_name, item.expected_base
            );
            continue;
        }
        println!(
            "  Updating PR #{} base to '{}'...",
            item.pr.number, item.expected_base
        );
        github.update_pr_base(owner, repo, item.pr.number, &item.expected_base)?;
    }

    // Phase 4: Update/create stack comments on all PRs
    if !dry_run {
        update_stack_comments(github, plan, &bookmark_to_pr)?;
    }

    Ok(())
}

/// Visible for testing only — not part of the public API.
fn update_stack_comments(
    github: &dyn GitHub,
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

        let body = comment::generate_comment_body(&entries, &plan.default_branch);

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
    use crate::github::types::{IssueComment, PullRequestRef, RepoInfo};
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

    impl GitHub for RecordingGitHub {
        fn find_open_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> {
            Ok(None)
        }
        fn create_pr(
            &self,
            _o: &str,
            _r: &str,
            _t: &str,
            _b: &str,
            head: &str,
            base: &str,
        ) -> Result<PullRequest> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("create_pr:{head}:{base}"));
            Ok(PullRequest {
                number: 42,
                html_url: "https://github.com/o/r/pull/42".to_string(),
                title: "test".to_string(),
                body: None,
                base: PullRequestRef {
                    ref_name: base.to_string(),
                },
                head: PullRequestRef {
                    ref_name: head.to_string(),
                },
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
            issue: u64,
            _b: &str,
        ) -> Result<IssueComment> {
            self.calls
                .lock().expect("poisoned")
                .push(format!("create_comment:#{issue}"));
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
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("testuser".to_string())
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
        fn get_branch_changes(&self, _to: &str) -> Result<Vec<LogEntry>> {
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
            existing_prs: HashMap::new(),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo {
                owner: "o".to_string(),
                repo: "r".to_string(),
            },
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
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

        impl GitHub for GitHubWithExistingComment {
            fn find_open_pr(
                &self,
                _o: &str,
                _r: &str,
                _h: &str,
            ) -> Result<Option<PullRequest>> {
                Ok(None)
            }
            fn create_pr(
                &self,
                _o: &str,
                _r: &str,
                _t: &str,
                _b: &str,
                _h: &str,
                _ba: &str,
            ) -> Result<PullRequest> {
                unimplemented!()
            }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
                unimplemented!()
            }
            fn request_reviewers(
                &self,
                _o: &str,
                _r: &str,
                _n: u64,
                _revs: &[String],
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
                    body: Some("<!-- stacker:stack-info -->\nold comment".to_string()),
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
            fn get_authenticated_user(&self) -> Result<String> {
                Ok("testuser".to_string())
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
            base: PullRequestRef {
                ref_name: "main".to_string(),
            },
            head: PullRequestRef {
                ref_name: "auth".to_string(),
            },
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![],
            existing_prs: HashMap::from([("auth".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo {
                owner: "o".to_string(),
                repo: "r".to_string(),
            },
            all_bookmarks: vec![make_bookmark("auth")],
            default_branch: "main".to_string(),
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
            base: PullRequestRef {
                ref_name: "main".to_string(),
            },
            head: PullRequestRef {
                ref_name: "profile".to_string(),
            },
        };

        let plan = SubmissionPlan {
            bookmarks_needing_push: vec![],
            bookmarks_needing_pr: vec![],
            bookmarks_needing_base_update: vec![super::super::plan::BookmarkNeedingBaseUpdate {
                bookmark: make_bookmark("profile"),
                pr: existing_pr.clone(),
                expected_base: "auth".to_string(),
            }],
            existing_prs: HashMap::from([("profile".to_string(), existing_pr)]),
            remote_name: "origin".to_string(),
            repo_info: RepoInfo {
                owner: "o".to_string(),
                repo: "r".to_string(),
            },
            all_bookmarks: vec![make_bookmark("profile")],
            default_branch: "main".to_string(),
        };

        execute_submission_plan(&jj, &github, &plan, &[], false).unwrap();

        assert!(github.calls().iter().any(|c| c == "update_base:#5:auth"));
    }
}
