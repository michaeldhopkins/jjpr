use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::forge::types::{PullRequest, RepoInfo};
use crate::forge::{Forge, ForgeKind};
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

/// What needs to happen for a bookmark whose PR body's managed section is stale.
#[derive(Debug)]
pub struct BookmarkNeedingBodyUpdate {
    pub bookmark: Bookmark,
    pub pr_number: u64,
    pub new_body: String,
}

/// What needs to happen for a draft PR that should be marked ready.
#[derive(Debug)]
pub struct BookmarkNeedingReady {
    pub bookmark: Bookmark,
    pub pr_number: u64,
}

/// A bookmark whose PR title doesn't match the current commit description.
#[derive(Debug)]
pub struct TitleDrift {
    pub bookmark: Bookmark,
    pub pr_number: u64,
    pub current_title: String,
    pub expected_title: String,
}

/// A bookmark whose PR was already merged/closed on GitHub.
#[derive(Debug)]
pub struct MergedBookmark {
    pub bookmark: Bookmark,
    pub pr_number: u64,
    pub html_url: String,
}

/// The full submission plan.
#[derive(Debug)]
pub struct SubmissionPlan {
    pub bookmarks_needing_push: Vec<Bookmark>,
    pub bookmarks_needing_pr: Vec<BookmarkNeedingPr>,
    pub bookmarks_needing_base_update: Vec<BookmarkNeedingBaseUpdate>,
    pub bookmarks_needing_body_update: Vec<BookmarkNeedingBodyUpdate>,
    pub bookmarks_needing_ready: Vec<BookmarkNeedingReady>,
    pub bookmarks_needing_reviewers: Vec<(Bookmark, u64)>,
    pub bookmarks_with_title_drift: Vec<TitleDrift>,
    pub bookmarks_already_merged: Vec<MergedBookmark>,
    pub existing_prs: HashMap<String, PullRequest>,
    pub remote_name: String,
    pub repo_info: RepoInfo,
    pub forge_kind: ForgeKind,
    pub all_bookmarks: Vec<Bookmark>,
    pub default_branch: String,
    pub draft: bool,
    pub stack_nav: crate::config::StackNavMode,
}

impl SubmissionPlan {
    /// Whether this plan has any actions that will modify remote state.
    pub fn has_actions(&self) -> bool {
        !self.bookmarks_needing_push.is_empty()
            || !self.bookmarks_needing_pr.is_empty()
            || !self.bookmarks_needing_base_update.is_empty()
            || !self.bookmarks_needing_body_update.is_empty()
            || !self.bookmarks_needing_ready.is_empty()
            || !self.bookmarks_needing_reviewers.is_empty()
    }
}

const DESCRIPTION_START: &str = "<!-- jjpr:description -->";
const DESCRIPTION_END: &str = "<!-- /jjpr:description -->";

/// Derive the PR title and raw body text from the first change in a segment.
fn derive_pr_title_body(segment: &NarrowedSegment) -> (String, String) {
    if let Some(change) = segment.changes.first() {
        let title = change.description_first_line.clone();
        let mut body = change
            .description
            .strip_prefix(&title)
            .unwrap_or("")
            .trim()
            .to_string();

        if !segment.merge_source_names.is_empty() {
            let note = generate_merge_note(&segment.merge_source_names);
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&note);
        }

        (title, body)
    } else {
        (segment.bookmark.name.clone(), String::new())
    }
}

fn generate_merge_note(source_names: &[String]) -> String {
    let formatted: Vec<String> = source_names.iter().map(|n| format!("`{n}`")).collect();
    let sources_text = match formatted.len() {
        1 => formatted[0].clone(),
        2 => format!("{} and {}", formatted[0], formatted[1]),
        _ => {
            let (last, rest) = formatted.split_last().unwrap();
            format!("{}, and {last}", rest.join(", "))
        }
    };
    let plural = if source_names.len() == 1 {
        "that PR is"
    } else {
        "those PRs are"
    };
    format!(
        "**Merge note:** This change also merges {sources_text} in jj. \
         The diff may include changes from {sources_text} until {plural} merged."
    )
}

/// Wrap commit body text in sentinel markers for the initial PR body.
pub fn wrap_managed_body(commit_body: &str) -> String {
    format!("{DESCRIPTION_START}\n{commit_body}\n{DESCRIPTION_END}")
}

/// Extract the managed section from a PR body, if sentinel markers are present.
pub fn extract_managed_body(pr_body: &str) -> Option<&str> {
    let start_idx = pr_body.find(DESCRIPTION_START)?;
    let content_start = start_idx + DESCRIPTION_START.len();
    let end_idx = pr_body[content_start..].find(DESCRIPTION_END)? + content_start;
    Some(pr_body[content_start..end_idx].trim())
}

/// Replace the managed section in a PR body, preserving everything outside the sentinels.
fn replace_managed_body(pr_body: &str, new_commit_body: &str) -> String {
    let Some(start_idx) = pr_body.find(DESCRIPTION_START) else {
        return pr_body.to_string();
    };
    let Some(end_tag_start) = pr_body[start_idx..].find(DESCRIPTION_END) else {
        return pr_body.to_string();
    };
    let end_idx = start_idx + end_tag_start + DESCRIPTION_END.len();

    let before = &pr_body[..start_idx];
    let after = &pr_body[end_idx..];
    format!("{before}{DESCRIPTION_START}\n{new_commit_body}\n{DESCRIPTION_END}{after}")
}

/// Options for building a submission plan.
pub struct SubmitOptions<'a> {
    pub draft: bool,
    pub ready: bool,
    pub reviewers: &'a [String],
    pub stack_base: Option<&'a str>,
    pub stack_nav: crate::config::StackNavMode,
}

/// Build a submission plan by comparing local state with forge state.
pub fn create_submission_plan(
    github: &dyn Forge,
    segments: &[NarrowedSegment],
    remote_name: &str,
    repo_info: &RepoInfo,
    forge_kind: ForgeKind,
    default_branch: &str,
    opts: &SubmitOptions<'_>,
) -> Result<SubmissionPlan> {
    let draft = opts.draft;
    let ready = opts.ready;
    let reviewers = opts.reviewers;
    let stack_base = opts.stack_base;
    let stack_nav = opts.stack_nav;
    // Batch: one API call for all open PRs instead of one per bookmark
    let all_open_prs = github
        .list_open_prs(&repo_info.owner, &repo_info.repo)
        .context("failed to list open PRs — check `jjpr auth test`")?;

    let pr_map = crate::forge::build_pr_map(all_open_prs, &repo_info.owner);

    let mut bookmarks_needing_push = Vec::new();
    let mut bookmarks_needing_pr = Vec::new();
    let mut bookmarks_needing_base_update = Vec::new();
    let mut bookmarks_needing_body_update = Vec::new();
    let mut bookmarks_needing_ready = Vec::new();
    let mut bookmarks_needing_reviewers = Vec::new();
    let mut bookmarks_with_title_drift = Vec::new();
    let mut bookmarks_already_merged = Vec::new();
    let mut existing_prs: HashMap<String, PullRequest> = HashMap::new();
    let mut all_bookmarks = Vec::new();

    for (i, segment) in segments.iter().enumerate() {
        let bookmark = &segment.bookmark;
        all_bookmarks.push(bookmark.clone());

        // Determine expected base branch
        let base_branch = if i == 0 {
            stack_base.unwrap_or(default_branch).to_string()
        } else {
            segments[i - 1].bookmark.name.clone()
        };

        let existing_pr = pr_map.get(&bookmark.name).cloned();

        if existing_pr.is_none() {
            // No open PR — check if it was already merged before doing anything else
            match github.find_merged_pr(&repo_info.owner, &repo_info.repo, &bookmark.name) {
                Ok(Some(merged_pr)) => {
                    bookmarks_already_merged.push(MergedBookmark {
                        bookmark: bookmark.clone(),
                        pr_number: merged_pr.number,
                        html_url: merged_pr.html_url,
                    });
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "  Warning: could not check merged status for '{}': {e}",
                        bookmark.name
                    );
                }
                Ok(None) => {}
            }
        }

        // Check if bookmark needs push (after merged check to avoid recreating deleted branches)
        if !bookmark.is_synced {
            bookmarks_needing_push.push(bookmark.clone());
        }

        if let Some(pr) = existing_pr {
            // Check if base needs updating
            if pr.base.ref_name != base_branch {
                bookmarks_needing_base_update.push(BookmarkNeedingBaseUpdate {
                    bookmark: bookmark.clone(),
                    pr: pr.clone(),
                    expected_base: base_branch,
                });
            }

            // Check if the managed body section is stale
            let (expected_title, expected_body) = derive_pr_title_body(segment);
            let current_body = pr.body.as_deref().unwrap_or("");
            if let Some(current_managed) = extract_managed_body(current_body)
                && current_managed != expected_body
            {
                let new_full_body = replace_managed_body(current_body, &expected_body);
                bookmarks_needing_body_update.push(BookmarkNeedingBodyUpdate {
                    bookmark: bookmark.clone(),
                    pr_number: pr.number,
                    new_body: new_full_body,
                });
            }

            // Check for title drift (only for single-commit segments — multi-commit
            // segments likely have manually curated PR titles)
            if segment.changes.len() == 1 && pr.title != expected_title {
                bookmarks_with_title_drift.push(TitleDrift {
                    bookmark: bookmark.clone(),
                    pr_number: pr.number,
                    current_title: pr.title.clone(),
                    expected_title,
                });
            }

            // Check if draft PR needs to be marked ready
            if ready && pr.draft {
                bookmarks_needing_ready.push(BookmarkNeedingReady {
                    bookmark: bookmark.clone(),
                    pr_number: pr.number,
                });
            }

            // Track reviewers needed on existing PRs
            if !reviewers.is_empty() {
                bookmarks_needing_reviewers.push((bookmark.clone(), pr.number));
            }

            existing_prs.insert(bookmark.name.clone(), pr);
        } else {
            let (title, body) = derive_pr_title_body(segment);

            bookmarks_needing_pr.push(BookmarkNeedingPr {
                bookmark: bookmark.clone(),
                base_branch,
                title,
                body: wrap_managed_body(&body),
            });
        }
    }

    Ok(SubmissionPlan {
        bookmarks_needing_push,
        bookmarks_needing_pr,
        bookmarks_needing_base_update,
        bookmarks_needing_body_update,
        bookmarks_needing_ready,
        bookmarks_needing_reviewers,
        bookmarks_with_title_drift,
        bookmarks_already_merged,
        existing_prs,
        remote_name: remote_name.to_string(),
        repo_info: repo_info.clone(),
        forge_kind,
        all_bookmarks,
        default_branch: default_branch.to_string(),
        draft,
        stack_nav,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::{ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequestRef, ReviewSummary};
    use crate::jj::types::LogEntry;

    struct StubGitHub {
        prs: HashMap<String, PullRequest>,
    }

    impl Forge for StubGitHub {
        fn list_open_prs(
            &self,
            _owner: &str,
            _repo: &str,
        ) -> Result<Vec<PullRequest>> {
            Ok(self.prs.values().cloned().collect())
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
        fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> {
            unimplemented!()
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
            unimplemented!()
        }
        fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> {
            unimplemented!()
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
                conflict: false,
            }],
            merge_source_names: vec![],
        }
    }

    fn make_pr(name: &str, base: &str) -> PullRequest {
        PullRequest {
            number: 1,
            html_url: "https://github.com/o/r/pull/1".to_string(),
            title: format!("Add {name}"),
            body: Some("Detailed description".to_string()),
            base: PullRequestRef { ref_name: base.to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: name.to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
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

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert_eq!(plan.bookmarks_needing_push.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
        assert_eq!(plan.bookmarks_needing_pr[0].title, "Add feature");
        assert_eq!(
            plan.bookmarks_needing_pr[0].body,
            wrap_managed_body("Detailed description")
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

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
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

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
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

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
        assert_eq!(plan.bookmarks_needing_pr[1].base_branch, "auth");
        assert_eq!(plan.bookmarks_needing_pr[2].base_branch, "profile");
    }

    #[test]
    fn test_plan_stale_title_does_not_trigger_body_update() {
        let mut pr = make_pr("feature", "main");
        pr.title = "Old title".to_string();
        // Body has sentinels with matching content — no update needed
        pr.body = Some(wrap_managed_body("Detailed description"));

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_plan_detects_title_drift() {
        let mut pr = make_pr("feature", "main");
        pr.title = "Old title".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert_eq!(plan.bookmarks_with_title_drift.len(), 1);
        assert_eq!(plan.bookmarks_with_title_drift[0].current_title, "Old title");
        assert_eq!(plan.bookmarks_with_title_drift[0].expected_title, "Add feature");
    }

    #[test]
    fn test_plan_tracks_reviewers_for_existing_prs() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };
        let reviewers = ["alice".to_string()];

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &reviewers, stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert_eq!(plan.bookmarks_needing_reviewers.len(), 1);
        assert_eq!(plan.bookmarks_needing_reviewers[0].1, 1); // pr number
    }

    #[test]
    fn test_plan_detects_stale_managed_body() {
        let mut pr = make_pr("feature", "main");
        pr.body = Some(wrap_managed_body("Old body text"));

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert_eq!(plan.bookmarks_needing_body_update.len(), 1);
        // The new body should contain the updated managed section
        assert!(extract_managed_body(&plan.bookmarks_needing_body_update[0].new_body)
            .is_some_and(|m| m == "Detailed description"));
    }

    #[test]
    fn test_plan_no_update_when_managed_body_matches() {
        let mut pr = make_pr("feature", "main");
        pr.body = Some(wrap_managed_body("Detailed description"));

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_plan_preserves_user_content_around_sentinels() {
        let mut pr = make_pr("feature", "main");
        let body_with_extras = format!(
            "User notes above\n\n{}\n\n## Screenshots\nSome screenshot",
            wrap_managed_body("Old body")
        );
        pr.body = Some(body_with_extras);

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert_eq!(plan.bookmarks_needing_body_update.len(), 1);
        let new_body = &plan.bookmarks_needing_body_update[0].new_body;
        assert!(new_body.starts_with("User notes above"));
        assert!(new_body.contains("## Screenshots\nSome screenshot"));
        assert!(extract_managed_body(new_body).is_some_and(|m| m == "Detailed description"));
    }

    #[test]
    fn test_plan_no_update_when_sentinels_removed() {
        let mut pr = make_pr("feature", "main");
        // User completely removed the sentinels from the body
        pr.body = Some("Completely rewritten body with no sentinels".to_string());

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_wrap_managed_body() {
        let wrapped = wrap_managed_body("hello world");
        assert_eq!(
            wrapped,
            "<!-- jjpr:description -->\nhello world\n<!-- /jjpr:description -->"
        );
    }

    #[test]
    fn test_extract_managed_body() {
        let body = "<!-- jjpr:description -->\nhello world\n<!-- /jjpr:description -->";
        assert_eq!(extract_managed_body(body), Some("hello world"));
    }

    #[test]
    fn test_extract_managed_body_with_surrounding_content() {
        let body = "User text\n\n<!-- jjpr:description -->\nmanaged\n<!-- /jjpr:description -->\n\nMore user text";
        assert_eq!(extract_managed_body(body), Some("managed"));
    }

    #[test]
    fn test_extract_managed_body_no_markers() {
        assert_eq!(extract_managed_body("plain text"), None);
    }

    #[test]
    fn test_extract_managed_body_only_start_marker() {
        let body = "text\n<!-- jjpr:description -->\nsome content but no end marker";
        assert_eq!(extract_managed_body(body), None);
    }

    #[test]
    fn test_replace_managed_body_preserves_surroundings() {
        let body = "Before\n<!-- jjpr:description -->\nold\n<!-- /jjpr:description -->\nAfter";
        let result = replace_managed_body(body, "new content");
        assert_eq!(
            result,
            "Before\n<!-- jjpr:description -->\nnew content\n<!-- /jjpr:description -->\nAfter"
        );
        assert_eq!(extract_managed_body(&result), Some("new content"));
    }

    #[test]
    fn test_replace_managed_body_no_markers() {
        let body = "no markers here";
        assert_eq!(replace_managed_body(body, "new"), body);
    }

    #[test]
    fn test_plan_skips_merged_prs() {
        struct GitHubWithMergedPr;

        impl Forge for GitHubWithMergedPr {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, head: &str) -> Result<Option<PullRequest>> {
                if head == "auth" {
                    Ok(Some(PullRequest {
                        number: 99,
                        html_url: "https://github.com/o/r/pull/99".to_string(),
                        title: "Add auth".to_string(),
                        body: None,
                        base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                        head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
                        draft: false,
                        node_id: String::new(),
                        merged_at: Some("2024-01-01T00:00:00Z".to_string()),
                        requested_reviewers: vec![],
                    }))
                } else {
                    Ok(None)
                }
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![
            make_segment("auth", true),
            make_segment("profile", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubWithMergedPr, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment },
        ).unwrap();

        assert_eq!(plan.bookmarks_already_merged.len(), 1);
        assert_eq!(plan.bookmarks_already_merged[0].bookmark.name, "auth");
        assert_eq!(plan.bookmarks_already_merged[0].pr_number, 99);
        // profile should still get a new PR
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].bookmark.name, "profile");
    }

    #[test]
    fn test_plan_does_not_skip_closed_but_unmerged_prs() {
        struct GitHubWithClosedPr;

        impl Forge for GitHubWithClosedPr {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, _head: &str) -> Result<Option<PullRequest>> {
                // Closed but not merged — merged_at is None, so find_merged_pr returns None
                Ok(None)
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubWithClosedPr, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment },
        ).unwrap();

        // A closed-but-not-merged PR should NOT be treated as merged
        assert!(plan.bookmarks_already_merged.is_empty());
        assert_eq!(plan.bookmarks_needing_pr.len(), 1, "should create a new PR");
    }

    #[test]
    fn test_plan_merged_bookmark_not_pushed() {
        struct GitHubWithMergedPr;

        impl Forge for GitHubWithMergedPr {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, head: &str) -> Result<Option<PullRequest>> {
                if head == "auth" {
                    Ok(Some(PullRequest {
                        number: 99,
                        html_url: "https://github.com/o/r/pull/99".to_string(),
                        title: "Add auth".to_string(),
                        body: None,
                        base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                        head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
                        draft: false,
                        node_id: String::new(),
                        merged_at: Some("2024-01-01T00:00:00Z".to_string()),
                        requested_reviewers: vec![],
                    }))
                } else {
                    Ok(None)
                }
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        // auth is not synced but already merged — should NOT be pushed
        let segments = vec![make_segment("auth", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubWithMergedPr, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment },
        ).unwrap();

        assert_eq!(plan.bookmarks_already_merged.len(), 1);
        assert!(
            plan.bookmarks_needing_push.is_empty(),
            "merged bookmarks should not be pushed: {:?}",
            plan.bookmarks_needing_push.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_plan_no_title_drift_when_title_matches() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert!(plan.bookmarks_with_title_drift.is_empty());
    }

    #[test]
    fn test_plan_no_title_drift_for_multi_commit_segment() {
        let mut pr = make_pr("feature", "main");
        pr.title = "Manually curated title".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let mut segment = make_segment("feature", true);
        segment.changes.push(LogEntry {
            commit_id: "c_extra".to_string(),
            change_id: "ch_extra".to_string(),
            author_name: "Test".to_string(),
            author_email: "test@test.com".to_string(),
            description: "Earlier commit".to_string(),
            description_first_line: "Earlier commit".to_string(),
            parents: vec![],
            local_bookmarks: vec![],
            remote_bookmarks: vec![],
            is_working_copy: false,
            conflict: false,
        });
        let segments = vec![segment];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert!(
            plan.bookmarks_with_title_drift.is_empty(),
            "multi-commit segments should not report title drift"
        );
    }

    #[test]
    fn test_plan_no_reviewers_tracked_when_empty() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert!(plan.bookmarks_needing_reviewers.is_empty());
    }

    #[test]
    fn test_plan_identifies_draft_prs_for_ready() {
        let mut pr = make_pr("feature", "main");
        pr.draft = true;
        pr.node_id = "PR_kwDOxyz".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        // With ready=false, no bookmarks_needing_ready
        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert!(plan.bookmarks_needing_ready.is_empty());

        // With ready=true, draft PR is identified
        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: true, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        assert_eq!(plan.bookmarks_needing_ready.len(), 1);
        assert_eq!(plan.bookmarks_needing_ready[0].pr_number, 1);
    }

    #[test]
    fn test_plan_filters_fork_prs() {
        let mut fork_pr = make_pr("feature", "main");
        fork_pr.head.label = "someone-else:feature".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), fork_pr)]),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();

        // Fork PR should be filtered out — treated as if no PR exists
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert!(plan.existing_prs.is_empty());
    }

    #[test]
    fn test_plan_accepts_prs_with_empty_label() {
        let mut pr = make_pr("feature", "main");
        pr.head.label = String::new();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();

        // Empty label (e.g. from test stubs) should pass through the filter
        assert!(plan.bookmarks_needing_pr.is_empty());
        assert_eq!(plan.existing_prs.len(), 1);
    }

    #[test]
    fn test_plan_error_context_on_list_failure() {
        struct FailingGitHub;
        impl Forge for FailingGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                anyhow::bail!("HTTP 401 Unauthorized")
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { unimplemented!() }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { unimplemented!() }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let err = create_submission_plan(&FailingGitHub, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("jjpr auth test"), "error should hint at auth: {msg}");
    }

    #[test]
    fn test_plan_warns_on_merged_check_failure() {
        struct MergedCheckFailsGitHub;
        impl Forge for MergedCheckFailsGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { unimplemented!() }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> {
                anyhow::bail!("network timeout")
            }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        // Should succeed (not abort) and plan a PR despite merged check failing
        let plan = create_submission_plan(
            &MergedCheckFailsGitHub, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment },
        ).unwrap();
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert!(plan.bookmarks_already_merged.is_empty());
    }

    #[test]
    fn test_plan_uses_stack_base_for_first_pr() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![
            make_segment("auth", false),
            make_segment("profile", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: Some("coworker-feat"), stack_nav: crate::config::StackNavMode::Comment },
        ).unwrap();
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "coworker-feat");
        assert_eq!(plan.bookmarks_needing_pr[1].base_branch, "auth");
    }

    #[test]
    fn test_plan_merge_note_in_pr_body() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let mut segment = make_segment("merge-feat", false);
        segment.merge_source_names = vec!["feat-d".to_string()];
        let segments = vec![segment];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        let body = &plan.bookmarks_needing_pr[0].body;
        assert!(body.contains("**Merge note:**"), "body should contain merge note: {body}");
        assert!(body.contains("`feat-d`"), "body should reference the merge source: {body}");
    }

    #[test]
    fn test_plan_no_merge_note_for_linear() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment }).unwrap();
        let body = &plan.bookmarks_needing_pr[0].body;
        assert!(!body.contains("Merge note"), "linear segment should have no merge note: {body}");
    }

    #[test]
    fn test_plan_merge_note_three_parents() {
        let note = generate_merge_note(&[
            "feat-b".to_string(),
            "feat-c".to_string(),
            "feat-d".to_string(),
        ]);
        assert!(note.contains("`feat-b`, `feat-c`, and `feat-d`"), "should format 3 sources: {note}");
        assert!(note.contains("those PRs are"), "should use plural: {note}");
    }

    #[test]
    fn test_generate_merge_note_single() {
        let note = generate_merge_note(&["feat-x".to_string()]);
        assert!(note.contains("`feat-x`"));
        assert!(note.contains("that PR is"));
    }

    #[test]
    fn test_generate_merge_note_two() {
        let note = generate_merge_note(&["feat-a".to_string(), "feat-b".to_string()]);
        assert!(note.contains("`feat-a` and `feat-b`"));
        assert!(note.contains("those PRs are"));
    }

    #[test]
    fn test_plan_falls_back_to_default_branch() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft: false, ready: false, reviewers: &[], stack_base: None, stack_nav: crate::config::StackNavMode::Comment },
        ).unwrap();
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
    }
}
