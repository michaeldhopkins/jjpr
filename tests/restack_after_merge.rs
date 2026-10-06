//! Issue #10: `submit` after the bottom of a stack was squash-merged out of band.
//!
//! The forge is a bare git repo. A second clone plays the forge's squash merge:
//! it lands the bottom's content on `main` as a new commit, so the bottom's own
//! commit never enters trunk. These tests drive the same jj and planning code
//! `jjpr submit` does, with a stub forge that reports the bottom merged.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use jjpr::forge::types::{
    ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequest, PullRequestRef,
    RepoInfo, ReviewSummary,
};
use jjpr::forge::{Forge, ForgeKind};
use jjpr::graph::change_graph;
use jjpr::jj::Jj;
use jjpr::jj::types::{Bookmark, NarrowedSegment};
use jjpr::submit::{analyze, plan, resolve, restack};

/// Reports the named branches as merged; every other branch has no PR.
struct MergedForge {
    merged: Vec<&'static str>,
    lookups: Mutex<Vec<String>>,
}

fn pr(number: u64, head: &str) -> PullRequest {
    let side = |r: &str| PullRequestRef {
        ref_name: r.to_string(),
        label: String::new(),
        sha: String::new(),
    };
    PullRequest {
        number,
        html_url: format!("https://github.com/o/r/pull/{number}"),
        title: head.to_string(),
        body: None,
        base: side("main"),
        head: side(head),
        draft: false,
        node_id: String::new(),
        merged_at: Some("2026-10-05T00:00:00Z".to_string()),
        requested_reviewers: vec![],
        author: String::new(),
        stack: None,
    }
}

impl Forge for MergedForge {
    fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
        Ok(vec![])
    }
    fn find_merged_pr(&self, _: &str, _: &str, head: &str) -> Result<Option<PullRequest>> {
        self.lookups.lock().unwrap().push(head.to_string());
        Ok(self.merged.contains(&head).then(|| pr(1, head)))
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
        unimplemented!()
    }
    fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> {
        unimplemented!()
    }
    fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> {
        unimplemented!()
    }
    fn create_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<IssueComment> {
        unimplemented!()
    }
    fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn delete_comment(&self, _: &str, _: &str, _: u64) -> Result<()> {
        unimplemented!()
    }
    fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
        unimplemented!()
    }
    fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> {
        unimplemented!()
    }
    fn get_authenticated_user(&self) -> Result<String> {
        unimplemented!()
    }
    fn merge_pr(&self, _: &str, _: &str, _: u64, _: MergeMethod) -> Result<()> {
        unimplemented!()
    }
    fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
        unimplemented!()
    }
    fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
        unimplemented!()
    }
    fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
        unimplemented!()
    }
    fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
        unimplemented!()
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{program} {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `bottom` and `top` stacked, both pushed; then the forge squash-merges
/// `bottom` into `main`, deleting its branch when `delete_branch` is set.
fn stack_with_squash_merged_bottom(delete_branch: bool) -> common::JjTestRepo {
    squash_merged_stack(delete_branch, "// bottom\n")
}

/// As above, with the forge landing `squashed` as `bottom.rs`: the reviewed
/// version, when it differs from what was pushed.
fn squash_merged_stack(delete_branch: bool, squashed: &str) -> common::JjTestRepo {
    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("bottom.rs", "// bottom\n", "Add bottom", "bottom");
    repo.commit_and_bookmark("top.rs", "// top\n", "Add top", "top");
    let mut push =
        jjpr::jj::version::push_new_bookmark_args(jjpr::jj::version::installed_jj_version())
            .to_vec();
    push.extend(["git", "push", "--remote", "origin"]);
    push.extend(["--bookmark", "bottom", "--bookmark", "top"]);
    repo.run_jj(&push);

    let forge_side = tempfile::TempDir::new().unwrap();
    let origin = repo.origin_path().to_str().unwrap().to_string();
    run(forge_side.path(), "jj", &["git", "clone", &origin, "clone"]);
    let clone = forge_side.path().join("clone");
    run(
        &clone,
        "jj",
        &["config", "set", "--repo", "user.email", "forge@jjpr.dev"],
    );
    run(&clone, "jj", &["new", "main"]);
    std::fs::write(clone.join("bottom.rs"), squashed).unwrap();
    run(&clone, "jj", &["commit", "-m", "Add bottom (#1)"]);
    run(&clone, "jj", &["bookmark", "set", "main", "-r", "@-"]);
    run(&clone, "jj", &["git", "push", "--bookmark", "main"]);
    if delete_branch {
        run(repo.origin_path(), "git", &["branch", "-D", "bottom"]);
    }
    repo
}

fn segments_for(jj: &dyn Jj, target: &str) -> Vec<NarrowedSegment> {
    let graph = change_graph::build_change_graph(jj).unwrap();
    let analysis = analyze::analyze_submission_graph(&graph, target).unwrap();
    resolve::resolve_bookmark_selections(&analysis.relevant_segments, false).unwrap()
}

fn plan_for(
    forge: &dyn Forge,
    segments: &[NarrowedSegment],
    dry_run: bool,
) -> plan::SubmissionPlan {
    plan::create_submission_plan(
        forge,
        segments,
        "origin",
        &RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        },
        ForgeKind::GitHub,
        "main",
        &plan::SubmitOptions {
            draft_mode: plan::DraftMode::Default,
            reviewers: &[],
            reviewer_scope: Default::default(),
            stack_base: None,
            stack_nav: Default::default(),
            dry_run,
        },
    )
    .unwrap()
}

/// Descriptions of the commits `top`'s PR would carry, newest first.
fn top_pr_commits(repo: &common::JjTestRepo) -> Vec<String> {
    repo.run_jj(&[
        "log",
        "--no-graph",
        "-r",
        "trunk()..top",
        "-T",
        "description.first_line() ++ \"\\n\"",
    ])
    .lines()
    .map(str::to_string)
    .collect()
}

/// The forge deleted `bottom`'s branch, so the fetch deletes the bookmark and
/// `bottom`'s commit folds into `top`'s segment: the bloated PR of issue #10.
#[test]
fn deleted_merged_bottom_is_dropped_from_the_survivor() {
    if !common::jj_available() {
        return;
    }
    let repo = stack_with_squash_merged_bottom(true);
    let mut jj = repo.runner();
    jj.set_fetch_remote(Some("origin".to_string()));
    let before: Vec<Bookmark> = jj.get_my_bookmarks().unwrap();
    jj.git_fetch().unwrap();

    let segments = segments_for(&jj, "top");
    assert_eq!(segments.len(), 1, "bottom's bookmark is gone");
    assert_eq!(
        top_pr_commits(&repo),
        vec!["Add top", "Add bottom"],
        "the bug"
    );

    let forge = MergedForge {
        merged: vec!["bottom"],
        lookups: Mutex::new(vec![]),
    };
    let plan = plan_for(&forge, &segments, false);
    let rewrote =
        restack::restack_merged_base(&jj, &forge, &plan, &segments, &before, false).unwrap();

    assert!(rewrote);
    assert_eq!(top_pr_commits(&repo), vec!["Add top"]);
    let rebuilt = restack::rebuild_segments(&jj, "top", &segments).unwrap();
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].changes.len(), 1);
    let stray = repo.run_jj(&[
        "log",
        "--no-graph",
        "-r",
        "all() ~ ::trunk()",
        "-T",
        "description.first_line() ++ \"\\n\"",
    ]);
    assert!(
        !stray.contains("Add bottom\n"),
        "the emptied bottom commit is abandoned: {stray}"
    );
    assert!(stray.contains("Add top"), "{stray}");
}

/// The forge kept `bottom`'s branch: the plan finds it merged, and `top` is
/// rebased off it onto trunk.
#[test]
fn kept_merged_bottom_rebases_the_survivor_onto_trunk() {
    if !common::jj_available() {
        return;
    }
    let repo = stack_with_squash_merged_bottom(false);
    let mut jj = repo.runner();
    jj.set_fetch_remote(Some("origin".to_string()));
    let before = jj.get_my_bookmarks().unwrap();
    jj.git_fetch().unwrap();

    let segments = segments_for(&jj, "top");
    assert_eq!(segments.len(), 2);
    let forge = MergedForge {
        merged: vec!["bottom"],
        lookups: Mutex::new(vec![]),
    };
    let plan = plan_for(&forge, &segments, false);
    assert_eq!(plan.bookmarks_already_merged.len(), 1);
    assert_eq!(
        top_pr_commits(&repo),
        vec!["Add top", "Add bottom"],
        "the bug"
    );

    let rewrote =
        restack::restack_merged_base(&jj, &forge, &plan, &segments, &before, false).unwrap();

    assert!(rewrote);
    assert_eq!(top_pr_commits(&repo), vec!["Add top"]);
}

/// A dry run, a foreign base, and a bottom that is not merged all leave the
/// stack alone.
#[test]
fn restack_leaves_the_stack_alone_when_it_should() {
    if !common::jj_available() {
        return;
    }
    let repo = stack_with_squash_merged_bottom(true);
    let mut jj = repo.runner();
    jj.set_fetch_remote(Some("origin".to_string()));
    let before = jj.get_my_bookmarks().unwrap();
    jj.git_fetch().unwrap();
    let segments = segments_for(&jj, "top");
    let merged = MergedForge {
        merged: vec!["bottom"],
        lookups: Mutex::new(vec![]),
    };
    let unmerged = MergedForge {
        merged: vec![],
        lookups: Mutex::new(vec![]),
    };

    let dry = plan_for(&merged, &segments, true);
    assert!(!restack::restack_merged_base(&jj, &merged, &dry, &segments, &before, false).unwrap());
    let real = plan_for(&merged, &segments, false);
    assert!(!restack::restack_merged_base(&jj, &merged, &real, &segments, &before, true).unwrap());
    let plan = plan_for(&unmerged, &segments, false);
    assert!(
        !restack::restack_merged_base(&jj, &unmerged, &plan, &segments, &before, false).unwrap()
    );
    assert!(
        unmerged
            .lookups
            .lock()
            .unwrap()
            .contains(&"bottom".to_string()),
        "the deleted bookmark was asked about"
    );
    assert!(
        !restack::restack_merged_base(&jj, &merged, &real, &segments, &[], false).unwrap(),
        "without the pre-fetch bookmarks a deleted bookmark cannot be seen"
    );
    assert_eq!(top_pr_commits(&repo), vec!["Add top", "Add bottom"]);
}

/// A merge-commit landing keeps `bottom`'s commit in trunk, so `top` already
/// sits on trunk: no rebase, which would only rewrite SHAs.
#[test]
fn merge_commit_landing_needs_no_restack() {
    if !common::jj_available() {
        return;
    }
    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("bottom.rs", "// bottom\n", "Add bottom", "bottom");
    repo.commit_and_bookmark("top.rs", "// top\n", "Add top", "top");
    // Fast-forward main over bottom, as a merge landing leaves it in trunk.
    repo.run_jj(&["bookmark", "set", "main", "-r", "bottom"]);
    repo.run_jj(&["git", "push", "--remote", "origin", "--bookmark", "main"]);
    let jj = repo.runner();
    let segments = segments_for(&jj, "top");
    let forge = MergedForge {
        merged: vec!["bottom"],
        lookups: Mutex::new(vec![]),
    };
    let plan = plan_for(&forge, &segments, false);
    let names: Vec<&str> = segments.iter().map(|s| s.bookmark.name.as_str()).collect();
    assert_eq!(names, vec!["top"], "bottom is in trunk now");
    assert!(!restack::restack_merged_base(&jj, &forge, &plan, &segments, &[], false).unwrap());
    assert_eq!(top_pr_commits(&repo), vec!["Add top"]);
}

/// The forge landed a different `bottom.rs` than the one pushed, so the
/// restack conflicts. Rebuilding the segments refuses before anything is
/// pushed, as the pre-flight does for any conflicted stack.
#[test]
fn a_restack_that_conflicts_is_refused_before_pushing() {
    if !common::jj_available() {
        return;
    }
    let repo = squash_merged_stack(true, "// bottom, as reviewed\n");
    let mut jj = repo.runner();
    jj.set_fetch_remote(Some("origin".to_string()));
    let before = jj.get_my_bookmarks().unwrap();
    jj.git_fetch().unwrap();
    let segments = segments_for(&jj, "top");
    let forge = MergedForge {
        merged: vec!["bottom"],
        lookups: Mutex::new(vec![]),
    };
    let plan = plan_for(&forge, &segments, false);

    assert!(restack::restack_merged_base(&jj, &forge, &plan, &segments, &before, false).unwrap());
    let err = restack::rebuild_segments(&jj, "top", &segments).unwrap_err();
    assert!(err.to_string().contains("unresolved conflicts"), "{err}");
}
