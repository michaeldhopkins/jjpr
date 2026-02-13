use std::collections::HashMap;

use anyhow::Result;

use crate::github::types::{PullRequest, RepoInfo};
use crate::github::GitHub;
use crate::jj::types::{Bookmark, NarrowedSegment};

/// What needs to happen for a bookmark that doesn't have a PR yet.
#[derive(Debug)]
pub struct BookmarkNeedingPr {
    pub bookmark: Bookmark,
    pub base_branch: String,
    pub title: String,
    pub body: String,
}

/// What needs to happen for a bookmark whose PR has the wrong base.
#[derive(Debug)]
pub struct BookmarkNeedingBaseUpdate {
    pub bookmark: Bookmark,
    pub pr: PullRequest,
    pub expected_base: String,
}

/// The full submission plan.
#[derive(Debug)]
pub struct SubmissionPlan {
    pub bookmarks_needing_push: Vec<Bookmark>,
    pub bookmarks_needing_pr: Vec<BookmarkNeedingPr>,
    pub bookmarks_needing_base_update: Vec<BookmarkNeedingBaseUpdate>,
    pub existing_prs: HashMap<String, PullRequest>,
    pub remote_name: String,
    pub repo_info: RepoInfo,
    pub all_bookmarks: Vec<Bookmark>,
    pub default_branch: String,
}

/// Build a submission plan by comparing local state with GitHub state.
pub fn create_submission_plan(
    github: &dyn GitHub,
    segments: &[NarrowedSegment],
    remote_name: &str,
    repo_info: &RepoInfo,
    default_branch: &str,
) -> Result<SubmissionPlan> {
    let mut bookmarks_needing_push = Vec::new();
    let mut bookmarks_needing_pr = Vec::new();
    let mut bookmarks_needing_base_update = Vec::new();
    let mut existing_prs: HashMap<String, PullRequest> = HashMap::new();
    let mut all_bookmarks = Vec::new();

    for (i, segment) in segments.iter().enumerate() {
        let bookmark = &segment.bookmark;
        all_bookmarks.push(bookmark.clone());

        // Determine expected base branch
        let base_branch = if i == 0 {
            default_branch.to_string()
        } else {
            segments[i - 1].bookmark.name.clone()
        };

        // Check if bookmark needs push
        if !bookmark.is_synced {
            bookmarks_needing_push.push(bookmark.clone());
        }

        // Check if PR exists
        let existing_pr =
            github.find_open_pr(&repo_info.owner, &repo_info.repo, &bookmark.name)?;

        if let Some(pr) = existing_pr {
            // Check if base needs updating
            if pr.base.ref_name != base_branch {
                bookmarks_needing_base_update.push(BookmarkNeedingBaseUpdate {
                    bookmark: bookmark.clone(),
                    pr: pr.clone(),
                    expected_base: base_branch,
                });
            }
            existing_prs.insert(bookmark.name.clone(), pr);
        } else {
            // Extract title and body from first change's description
            let (title, body) = if let Some(change) = segment.changes.first() {
                let title = change.description_first_line.clone();
                let body = change
                    .description
                    .strip_prefix(&title)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                (title, body)
            } else {
                (bookmark.name.clone(), String::new())
            };

            bookmarks_needing_pr.push(BookmarkNeedingPr {
                bookmark: bookmark.clone(),
                base_branch,
                title,
                body,
            });
        }
    }

    Ok(SubmissionPlan {
        bookmarks_needing_push,
        bookmarks_needing_pr,
        bookmarks_needing_base_update,
        existing_prs,
        remote_name: remote_name.to_string(),
        repo_info: repo_info.clone(),
        all_bookmarks,
        default_branch: default_branch.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::{IssueComment, PullRequestRef};
    use crate::jj::types::LogEntry;

    struct StubGitHub {
        prs: HashMap<String, PullRequest>,
    }

    impl GitHub for StubGitHub {
        fn find_open_pr(
            &self,
            _owner: &str,
            _repo: &str,
            head: &str,
        ) -> Result<Option<PullRequest>> {
            Ok(self.prs.get(head).cloned())
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
        fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> {
            unimplemented!()
        }
        fn create_comment(
            &self,
            _o: &str,
            _r: &str,
            _i: u64,
            _b: &str,
        ) -> Result<IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("testuser".to_string())
        }
    }

    fn make_segment(name: &str, synced: bool) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.to_string(),
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                has_remote: synced,
                is_synced: synced,
            },
            changes: vec![LogEntry {
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                author_name: "Test".to_string(),
                author_email: "test@test.com".to_string(),
                description: format!("Add {name}\n\nDetailed description"),
                description_first_line: format!("Add {name}"),
                parents: vec![],
                local_bookmarks: vec![name.to_string()],
                remote_bookmarks: vec![],
                is_working_copy: false,
            }],
        }
    }

    fn make_pr(name: &str, base: &str) -> PullRequest {
        PullRequest {
            number: 1,
            html_url: format!("https://github.com/o/r/pull/1"),
            title: format!("Add {name}"),
            body: None,
            base: PullRequestRef {
                ref_name: base.to_string(),
            },
            head: PullRequestRef {
                ref_name: name.to_string(),
            },
        }
    }

    #[test]
    fn test_plan_new_pr_needed() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, "main").unwrap();
        assert_eq!(plan.bookmarks_needing_push.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
        assert_eq!(plan.bookmarks_needing_pr[0].title, "Add feature");
        assert_eq!(
            plan.bookmarks_needing_pr[0].body,
            "Detailed description"
        );
    }

    #[test]
    fn test_plan_existing_pr_correct_base() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, "main").unwrap();
        assert!(plan.bookmarks_needing_push.is_empty());
        assert!(plan.bookmarks_needing_pr.is_empty());
        assert!(plan.bookmarks_needing_base_update.is_empty());
        assert_eq!(plan.existing_prs.len(), 1);
    }

    #[test]
    fn test_plan_existing_pr_wrong_base() {
        let gh = StubGitHub {
            prs: HashMap::from([("profile".to_string(), make_pr("profile", "main"))]),
        };
        // Stack: auth -> profile. Profile's base should be "auth", not "main"
        let segments = vec![
            make_segment("auth", true),
            make_segment("profile", true),
        ];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, "main").unwrap();
        assert_eq!(plan.bookmarks_needing_base_update.len(), 1);
        assert_eq!(
            plan.bookmarks_needing_base_update[0].expected_base,
            "auth"
        );
    }

    #[test]
    fn test_plan_stacked_base_branches() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![
            make_segment("auth", false),
            make_segment("profile", false),
            make_segment("settings", false),
        ];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, "main").unwrap();
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
        assert_eq!(plan.bookmarks_needing_pr[1].base_branch, "auth");
        assert_eq!(plan.bookmarks_needing_pr[2].base_branch, "profile");
    }
}
