mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use jjpr::forge::{AuthScheme, Forge, ForgeClient, ForgeKind, GitHubForge, PaginationStyle};
use jjpr::forge::types::RepoInfo;
use jjpr::graph::change_graph;
use jjpr::identity::Identity;
use jjpr::submit::{analyze, execute, plan, resolve};

use tempfile::TempDir;

const OWNER: &str = "michaeldhopkins";
const REPO: &str = "jjpr-testing-environment";

/// E2E test context: clones the testing repo, provides helpers, cleans up on Drop.
struct E2eContext {
    prefix: String,
    _parent: TempDir,
    repo_path: PathBuf,
}

impl E2eContext {
    fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_secs();
        let prefix = format!("t{:06x}", ts & 0xFFFFFF);

        let parent = TempDir::new().expect("create temp dir");
        let repo_path = parent.path().join("repo");
        let dest = repo_path.to_str().expect("non-utf8 path");

        let remote_url = format!("git@github.com:{OWNER}/{REPO}.git");
        let output = Command::new("jj")
            .args(["git", "clone", "--colocate", &remote_url, dest])
            .output()
            .expect("jj git clone");
        assert!(
            output.status.success(),
            "jj git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Don't override user config — let mine() match the real user's commits.

        Self {
            prefix,
            _parent: parent,
            repo_path,
        }
    }

    fn bookmark_name(&self, name: &str) -> String {
        format!("{}-{}", self.prefix, name)
    }

    fn write_file(&self, name: &str, content: &str) {
        std::fs::write(self.repo_path.join(name), content).expect("write");
    }

    fn commit(&self, message: &str) {
        run_jj(&self.repo_path, &["commit", "-m", message]);
    }

    fn set_bookmark(&self, name: &str) {
        run_jj(&self.repo_path, &["bookmark", "set", name, "-r", "@-"]);
    }

    fn runner(&self) -> jjpr::jj::JjRunner {
        jjpr::jj::JjRunner::new(self.repo_path.clone()).expect("create JjRunner")
    }

    /// Create a commit authored under `email` (simulating another machine's git
    /// identity — distinct from the local `user.email`), containing `file`, and
    /// bookmark it. `jj new` under `email` stamps the author; `jj commit`
    /// finalizes the working-copy file into that described commit.
    fn commit_as(&self, email: &str, file: &str, content: &str, msg: &str, bookmark: &str) {
        // Base on trunk() directly, not the clone's empty working-copy commit,
        // so no empty/undescribed commit lands between main and the branch.
        let cfg = format!("--config=user.email={email}");
        run_jj(&self.repo_path, &["new", "trunk()", &cfg]);
        self.write_file(file, content);
        run_jj(&self.repo_path, &["commit", &cfg, "-m", msg]);
        run_jj(&self.repo_path, &["bookmark", "set", bookmark, "-r", "@-"]);
    }

    fn local_email(&self) -> String {
        run_jj(&self.repo_path, &["config", "get", "user.email"]).trim().to_string()
    }
}

impl Drop for E2eContext {
    fn drop(&mut self) {
        let full_repo = format!("{OWNER}/{REPO}");

        // Close PRs with our prefix
        if let Ok(output) = Command::new("gh")
            .args([
                "pr", "list", "--repo", &full_repo,
                "--json", "number,headRefName",
                "--state", "open", "--limit", "50",
            ])
            .output()
            && let Ok(prs) =
                serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
        {
            for pr in &prs {
                let head = pr["headRefName"].as_str().unwrap_or("");
                if head.starts_with(&self.prefix) {
                    let number = pr["number"].as_u64().unwrap_or(0);
                    if number > 0 {
                        let _ = Command::new("gh")
                            .args([
                                "pr", "close", &number.to_string(),
                                "--repo", &full_repo,
                            ])
                            .output();
                    }
                }
            }
        }

        // Delete remote branches with our prefix
        if let Ok(output) = Command::new("gh")
            .args([
                "api",
                &format!(
                    "repos/{full_repo}/git/matching-refs/heads/{}",
                    self.prefix
                ),
            ])
            .output()
            && let Ok(refs) =
                serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
        {
            for r in &refs {
                if let Some(ref_name) = r["ref"].as_str() {
                    let _ = Command::new("gh")
                        .args([
                            "api",
                            &format!("repos/{full_repo}/git/{ref_name}"),
                            "-X", "DELETE",
                        ])
                        .output();
                }
            }
        }
    }
}

fn run_jj(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run jj");
    assert!(
        output.status.success(),
        "jj {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn find_pr(head: &str) -> Option<serde_json::Value> {
    let full_repo = format!("{OWNER}/{REPO}");
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            &full_repo,
            "--head",
            head,
            "--json",
            "number,title,baseRefName,headRefName",
            "--state",
            "open",
        ])
        .output()
        .expect("gh pr list");

    let prs: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).ok()?;
    prs.into_iter().next()
}

fn fetch_pr_body(pr_number: u64) -> String {
    let full_repo = format!("{OWNER}/{REPO}");
    let output = Command::new("gh")
        .args([
            "pr", "view", &pr_number.to_string(),
            "--repo", &full_repo,
            "--json", "body", "--jq", ".body",
        ])
        .output()
        .expect("gh pr view body");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn set_pr_body(pr_number: u64, body: &str) {
    let full_repo = format!("{OWNER}/{REPO}");
    let status = Command::new("gh")
        .args([
            "pr", "edit", &pr_number.to_string(),
            "--repo", &full_repo,
            "--body", body,
        ])
        .status()
        .expect("gh pr edit body");
    assert!(status.success(), "gh pr edit --body should succeed");
}

fn list_comments(pr_number: u64) -> Vec<serde_json::Value> {
    let full_repo = format!("{OWNER}/{REPO}");
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{full_repo}/issues/{pr_number}/comments"),
        ])
        .output()
        .expect("gh api list comments");

    serde_json::from_slice(&output.stdout).unwrap_or_default()
}

// --- E2E Tests (guarded by JJPR_E2E env var) ---

/// The GraphQL batch caps each connection at 100 nodes. A PR with more check
/// contexts than that must be topped up from REST, which paginates without
/// limit — otherwise the batch would silently decide CI from an arbitrary 100.
///
/// Exercised against a real public PR known to carry >100 contexts rather than
/// a fixture, because the whole point is the live batch→truncation→refill chain.
/// The target PR will eventually merge; if it 404s, pick another with >100
/// contexts via the statusCheckRollup query in the docs.
#[test]
fn test_batch_refills_checks_beyond_the_graphql_page_cap() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }

    // denoland/deno #31518 carried 136 check contexts when this was written.
    let (owner, repo, pr_number) = ("denoland", "deno", 31518u64);

    let token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None)
        .expect("GitHub token required for E2E tests");
    let client =
        ForgeClient::new("https://api.github.com", token, AuthScheme::Bearer, PaginationStyle::LinkHeader);
    let github = GitHubForge::new(client);

    // The head sha the batch keys checks off. Skip cleanly if the PR is gone.
    let Ok(prs) = github.list_open_prs(owner, repo) else {
        eprintln!("could not list PRs; skipping");
        return;
    };
    let Some(pr) = prs.into_iter().find(|p| p.number == pr_number) else {
        eprintln!("PR #{pr_number} no longer open; pick another >100-context PR");
        return;
    };

    // Guard against silent rot: if this PR's check count has fallen below the
    // 100-node page, the test would still pass without ever touching the refill.
    // Count the raw check-runs the same way the fix does, and skip loudly if the
    // PR no longer exceeds the cap.
    let count_token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None)
        .expect("GitHub token required for E2E tests");
    let counter =
        ForgeClient::new("https://api.github.com", count_token, AuthScheme::Bearer, PaginationStyle::LinkHeader);
    let encoded = jjpr::forge::http::url_encode(pr.checks_ref());
    let runs = counter
        .get_paginated_envelope(
            &format!("repos/{owner}/{repo}/commits/{encoded}/check-runs?per_page=100"),
            "check_runs",
        )
        .expect("counting check-runs must succeed");
    if runs.len() <= 100 {
        eprintln!("PR #{pr_number} now has {} check-runs (<=100); refill not exercised — pick another PR", runs.len());
        return;
    }

    let batched = github
        .batch_pr_status(owner, repo, &[(pr_number, pr.checks_ref().to_string())])
        .expect("GitHub should batch");
    let bundle = batched.get(&pr_number).expect("PR must be in the batch");

    // The refill must have populated checks — a truncated batch that gave up
    // would leave this None, and CI status would silently vanish.
    let batched_checks = bundle
        .checks
        .clone()
        .expect("checks must be present after refill");

    // And it must equal what full REST pagination sees. If the refill were
    // skipped, the batch's first-100 view could disagree with the true verdict.
    let rest_checks = github
        .get_pr_checks_status(owner, repo, pr.checks_ref())
        .expect("REST checks must succeed");

    assert_eq!(
        batched_checks, rest_checks,
        "refilled batch checks must match full REST pagination",
    );
}

#[test]
fn test_submit_creates_stacked_prs() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }
    if !common::jj_available() {
        println!("Skipping E2E test (jj not available)");
        return;
    }

    let ctx = E2eContext::new();
    let auth_name = ctx.bookmark_name("auth");
    let profile_name = ctx.bookmark_name("profile");

    // Build a 2-bookmark stack
    ctx.write_file(&format!("{auth_name}.rs"), "// auth module\n");
    ctx.commit("Add authentication\n\nImplements basic auth flow");
    ctx.set_bookmark(&auth_name);

    ctx.write_file(&format!("{profile_name}.rs"), "// profile module\n");
    ctx.commit("Add user profile\n\nProfile page implementation");
    ctx.set_bookmark(&profile_name);

    // Build graph and submit
    let jj = ctx.runner();
    let token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None)
        .expect("GitHub token required for E2E tests");
    let client = ForgeClient::new("https://api.github.com", token, AuthScheme::Bearer, PaginationStyle::LinkHeader);
    let github = GitHubForge::new(client);

    let graph = change_graph::build_change_graph(&jj).unwrap();
    let analysis =
        analyze::analyze_submission_graph(&graph, &profile_name).unwrap();
    assert_eq!(
        analysis.relevant_segments.len(),
        2,
        "should have 2 segments in stack"
    );

    let segments = resolve::resolve_bookmark_selections(
        &analysis.relevant_segments,
        false,
    )
    .unwrap();

    let repo_info = RepoInfo {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    };
    let submission_plan = plan::create_submission_plan(
        &github, &segments, "origin", &repo_info, ForgeKind::GitHub, "main",
        &plan::SubmitOptions {
            draft_mode: plan::DraftMode::Default,
            reviewers: &[],
            reviewer_scope: jjpr::forge::types::ReviewerScope::Bottom,
            stack_base: None,
            stack_nav: jjpr::config::StackNavMode::Comment,
            dry_run: false,
        },
    )
    .unwrap();

    assert_eq!(submission_plan.bookmarks_needing_push.len(), 2);
    assert_eq!(submission_plan.bookmarks_needing_pr.len(), 2);
    assert_eq!(submission_plan.bookmarks_needing_pr[0].base_branch, "main");
    assert_eq!(
        submission_plan.bookmarks_needing_pr[1].base_branch,
        auth_name
    );

    execute::execute_submission_plan(&jj, &github, &submission_plan).unwrap();

    // Verify PRs exist with correct bases
    let auth_pr = find_pr(&auth_name);
    assert!(auth_pr.is_some(), "auth PR should exist");
    let auth_pr = auth_pr.unwrap();
    assert_eq!(auth_pr["baseRefName"].as_str().unwrap(), "main");
    assert_eq!(
        auth_pr["title"].as_str().unwrap(),
        "Add authentication"
    );

    let profile_pr = find_pr(&profile_name);
    assert!(profile_pr.is_some(), "profile PR should exist");
    let profile_pr = profile_pr.unwrap();
    assert_eq!(
        profile_pr["baseRefName"].as_str().unwrap(),
        auth_name
    );
    assert_eq!(
        profile_pr["title"].as_str().unwrap(),
        "Add user profile"
    );

    // Verify stack comments exist on both PRs
    let auth_comments =
        list_comments(auth_pr["number"].as_u64().unwrap());
    assert!(
        auth_comments
            .iter()
            .any(|c| c["body"]
                .as_str()
                .unwrap_or("")
                .contains("<!-- jjpr:stack-info -->")),
        "auth PR should have stack comment"
    );

    let profile_comments =
        list_comments(profile_pr["number"].as_u64().unwrap());
    assert!(
        profile_comments
            .iter()
            .any(|c| c["body"]
                .as_str()
                .unwrap_or("")
                .contains("<!-- jjpr:stack-info -->")),
        "profile PR should have stack comment"
    );
}

/// Verifies the description-preservation fix end-to-end on a real forge:
/// 1. A submitted PR's managed body has its git trailer stripped and a
///    fingerprint recorded.
/// 2. A description hand-edited on the forge survives a re-submit (the
///    commit is unchanged, so jjpr must leave the edit alone rather than
///    overwrite it with the commit-derived body).
#[test]
fn test_submit_preserves_hand_edited_description() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }
    if !common::jj_available() {
        println!("Skipping E2E test (jj not available)");
        return;
    }

    let ctx = E2eContext::new();
    let name = ctx.bookmark_name("preserve");

    ctx.write_file(&format!("{name}.rs"), "// preserve module\n");
    ctx.commit(
        "Add preserve module\n\nReal body paragraph that must survive.\n\nCo-authored-by: Test User <test@example.com>",
    );
    ctx.set_bookmark(&name);

    let jj = ctx.runner();
    let token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None)
        .expect("GitHub token required for E2E tests");
    let github = || {
        let client = ForgeClient::new(
            "https://api.github.com",
            token.clone(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        GitHubForge::new(client)
    };
    let repo_info = RepoInfo {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    };
    let submit = || {
        let graph = change_graph::build_change_graph(&jj).unwrap();
        let analysis =
            analyze::analyze_submission_graph(&graph, &name).unwrap();
        let segments = resolve::resolve_bookmark_selections(
            &analysis.relevant_segments,
            false,
        )
        .unwrap();
        let p = plan::create_submission_plan(
            &github(), &segments, "origin", &repo_info, ForgeKind::GitHub,
            "main",
            &plan::SubmitOptions {
                draft_mode: plan::DraftMode::Default,
                reviewers: &[],
                reviewer_scope: jjpr::forge::types::ReviewerScope::Bottom,
                stack_base: None,
                stack_nav: jjpr::config::StackNavMode::Comment,
                dry_run: false,
            },
        )
        .unwrap();
        execute::execute_submission_plan(&jj, &github(), &p).unwrap();
    };

    // First submit creates the PR.
    submit();
    let pr = find_pr(&name).expect("preserve PR exists");
    let pr_number = pr["number"].as_u64().unwrap();

    // Commit B: the Co-authored-by trailer is stripped; the real body and a
    // fingerprint marker are present.
    let created_body = fetch_pr_body(pr_number);
    assert!(
        created_body.contains("Real body paragraph that must survive."),
        "managed body should carry the commit body, was:\n{created_body}"
    );
    assert!(
        !created_body.contains("Co-authored-by"),
        "trailer should be stripped from the PR body, was:\n{created_body}"
    );
    assert!(
        created_body.contains("<!-- jjpr:body-fp "),
        "PR body should carry a fingerprint marker, was:\n{created_body}"
    );

    // Simulate a user editing the description directly on the forge, inside
    // the sentinels, leaving the fingerprint in place.
    let edited_body = created_body.replace(
        "Real body paragraph that must survive.",
        "HAND EDITED DO NOT CLOBBER",
    );
    set_pr_body(pr_number, &edited_body);

    // Re-submit with the commit unchanged. The fix: jjpr must not overwrite
    // the hand edit.
    submit();

    let final_body = fetch_pr_body(pr_number);
    assert!(
        final_body.contains("HAND EDITED DO NOT CLOBBER"),
        "hand-edited description must survive re-submit, was:\n{final_body}"
    );
    assert!(
        !final_body.contains("Real body paragraph that must survive."),
        "jjpr must not have reverted the description to the commit body, was:\n{final_body}"
    );
}

/// Verifies that once the bottom PR of a stack is merged on the forge and
/// the local bookmark is cleaned up, a re-submit places the merged PR in
/// the `<details>` fossil block of the remaining open PR's comment, with
/// strikethrough rendering and no icon. Exercises the full data flow:
/// previous JJPR_DATA → classify_stack_entries → generate_comment_body.
#[test]
fn test_merged_bottom_renders_in_fossil_details_block() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }
    if !common::jj_available() {
        println!("Skipping E2E test (jj not available)");
        return;
    }

    let ctx = E2eContext::new();
    let bottom_name = ctx.bookmark_name("bottom");
    let top_name = ctx.bookmark_name("top");
    let full_repo = format!("{OWNER}/{REPO}");

    // Build a 2-bookmark stack
    ctx.write_file(&format!("{bottom_name}.rs"), "// bottom module\n");
    ctx.commit("Add bottom\n\nBottom of the stack");
    ctx.set_bookmark(&bottom_name);

    ctx.write_file(&format!("{top_name}.rs"), "// top module\n");
    ctx.commit("Add top\n\nTop of the stack");
    ctx.set_bookmark(&top_name);

    let jj = ctx.runner();
    let token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None)
        .expect("GitHub token required for E2E tests");
    let github = || {
        let client = ForgeClient::new(
            "https://api.github.com",
            token.clone(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        GitHubForge::new(client)
    };
    let repo_info = RepoInfo {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    };
    let opts = || plan::SubmitOptions {
        draft_mode: plan::DraftMode::Default,
        reviewers: &[],
        reviewer_scope: jjpr::forge::types::ReviewerScope::Bottom,
        stack_base: None,
        stack_nav: jjpr::config::StackNavMode::Comment,
        dry_run: false,
    };

    // First submit: both PRs created, both should have stack comments.
    {
        let graph = change_graph::build_change_graph(&jj).unwrap();
        let analysis =
            analyze::analyze_submission_graph(&graph, &top_name).unwrap();
        let segments = resolve::resolve_bookmark_selections(
            &analysis.relevant_segments,
            false,
        )
        .unwrap();
        let plan = plan::create_submission_plan(
            &github(), &segments, "origin", &repo_info, ForgeKind::GitHub,
            "main", &opts(),
        )
        .unwrap();
        execute::execute_submission_plan(&jj, &github(), &plan)
            .unwrap();
    }

    let bottom_pr = find_pr(&bottom_name).expect("bottom PR exists");
    let top_pr = find_pr(&top_name).expect("top PR exists");
    let bottom_number = bottom_pr["number"].as_u64().unwrap();
    let top_number = top_pr["number"].as_u64().unwrap();

    // Squash-merge the bottom PR. --admin bypasses required-review rules
    // on the test repo. We deliberately leave the remote branch in place
    // so PR #top keeps its base ref valid; that's the realistic end state
    // a user lands in before re-running submit.
    let merge_status = Command::new("gh")
        .args([
            "pr", "merge", &bottom_number.to_string(),
            "--repo", &full_repo,
            "--squash", "--admin",
        ])
        .status()
        .expect("gh pr merge");
    assert!(merge_status.success(), "gh pr merge should succeed");

    // Refresh local state so plan.create_submission_plan sees the merged
    // status when it queries the forge.
    run_jj(ctx.repo_path.as_path(), &["git", "fetch"]);

    // Second submit: top is now standalone. The previous comment had
    // [bottom, top]; classify must recognize bottom as a fossil and
    // render it in the <details> block.
    {
        let graph = change_graph::build_change_graph(&jj).unwrap();
        let analysis =
            analyze::analyze_submission_graph(&graph, &top_name).unwrap();
        let segments = resolve::resolve_bookmark_selections(
            &analysis.relevant_segments,
            false,
        )
        .unwrap();
        let plan = plan::create_submission_plan(
            &github(), &segments, "origin", &repo_info, ForgeKind::GitHub,
            "main", &opts(),
        )
        .unwrap();
        execute::execute_submission_plan(&jj, &github(), &plan)
            .unwrap();
    }

    // Inspect top's stack comment.
    let top_comments = list_comments(top_number);
    let stack_comment = top_comments
        .iter()
        .find(|c| {
            c["body"]
                .as_str()
                .unwrap_or("")
                .contains("<!-- jjpr:stack-info -->")
        })
        .expect("top PR should still have a stack comment");
    let body = stack_comment["body"].as_str().unwrap();

    assert!(
        body.contains("<details>"),
        "expected fossil <details> block, body was:\n{body}"
    );
    assert!(
        body.contains("earlier closed/merged"),
        "expected fossil summary text, body was:\n{body}"
    );
    assert!(
        body.contains(&format!("~~[`{bottom_name}`]")),
        "expected strikethrough fossil link for {bottom_name}, body was:\n{body}"
    );
    // No icon — fossils render as plain strikethrough now.
    assert!(
        !body.contains(":white_check_mark:"),
        "fossil rendering must not include the old white_check_mark icon: \n{body}"
    );
    // Top is still live; should not be strikethrough'd.
    assert!(
        !body.contains(&format!("~~[`{top_name}`]")),
        "top PR is still live and should not be strikethrough"
    );
}

/// The squash-merge work scenario: a user watches the top of a stack; the bottom
/// PR is squash-merged on the forge. The watched target (top) must stay findable
/// so watch keeps reconciling the SAME stack — it must not lose the target or
/// silently hijack to whatever the working copy happens to be on. Settles the
/// "commit switched during watch" decision against the real forge: because the
/// target survives the squash merge, the working-copy-inference fallback never
/// needs to fire.
#[test]
fn test_watch_target_findable_through_bottom_squash_merge() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }
    if !common::jj_available() {
        println!("Skipping E2E test (jj not available)");
        return;
    }

    let ctx = E2eContext::new();
    let bottom_name = ctx.bookmark_name("sqbot");
    let top_name = ctx.bookmark_name("sqtop");
    let full_repo = format!("{OWNER}/{REPO}");

    ctx.write_file(&format!("{bottom_name}.rs"), "// bottom module\n");
    ctx.commit("Add bottom\n\nBottom of the stack");
    ctx.set_bookmark(&bottom_name);
    ctx.write_file(&format!("{top_name}.rs"), "// top module\n");
    ctx.commit("Add top\n\nTop of the stack");
    ctx.set_bookmark(&top_name);

    let jj = ctx.runner();
    let token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None)
        .expect("GitHub token required for E2E tests");
    let github = || {
        let client = ForgeClient::new(
            "https://api.github.com",
            token.clone(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        GitHubForge::new(client)
    };
    let repo_info = RepoInfo {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    };
    let opts = plan::SubmitOptions {
        draft_mode: plan::DraftMode::Default,
        reviewers: &[],
        reviewer_scope: jjpr::forge::types::ReviewerScope::Bottom,
        stack_base: None,
        stack_nav: jjpr::config::StackNavMode::Comment,
        dry_run: false,
    };

    // Submit the 2-PR stack.
    {
        let graph = change_graph::build_change_graph(&jj).unwrap();
        let analysis = analyze::analyze_submission_graph(&graph, &top_name).unwrap();
        let segments =
            resolve::resolve_bookmark_selections(&analysis.relevant_segments, false).unwrap();
        let plan = plan::create_submission_plan(
            &github(), &segments, "origin", &repo_info, ForgeKind::GitHub, "main", &opts,
        )
        .unwrap();
        execute::execute_submission_plan(&jj, &github(), &plan).unwrap();
    }

    // Top starts based on the bottom branch.
    let top_pr = find_pr(&top_name).expect("top PR exists");
    assert_eq!(
        top_pr["baseRefName"].as_str().unwrap(),
        bottom_name,
        "top should start based on the bottom branch"
    );
    let bottom_number = find_pr(&bottom_name).expect("bottom PR exists")["number"]
        .as_u64()
        .unwrap();

    // Squash-merge the bottom PR (--admin bypasses required-review on the test repo).
    let merge_status = Command::new("gh")
        .args([
            "pr", "merge", &bottom_number.to_string(),
            "--repo", &full_repo, "--squash", "--admin",
        ])
        .status()
        .expect("gh pr merge");
    assert!(merge_status.success(), "squash-merge should succeed");

    // Fetch, as a watch poll would before it reconciles.
    run_jj(ctx.repo_path.as_path(), &["git", "fetch"]);

    // The decision: after a REAL squash merge, the watched target stays findable
    // and resolves to the same stack. If this ever regressed, watch would fall
    // back to inferring from the working copy — the hijack we're removing.
    let graph = change_graph::build_change_graph(&jj).unwrap();
    let analysis = analyze::analyze_submission_graph(&graph, &top_name)
        .expect("target must stay findable after a bottom squash-merge — watch must not lose it");
    let segments =
        resolve::resolve_bookmark_selections(&analysis.relevant_segments, false).unwrap();
    assert!(
        segments.iter().any(|s| s.bookmark.name == top_name),
        "resolved stack must still contain the top target after the squash merge"
    );
}

fn github_forge() -> GitHubForge {
    let token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None)
        .expect("GitHub token required for E2E tests");
    let client = ForgeClient::new(
        "https://api.github.com",
        token,
        AuthScheme::Bearer,
        PaginationStyle::LinkHeader,
    );
    GitHubForge::new(client)
}

/// Tier 1 (login match): a PR authored by YOU (your forge login) but committed
/// under a DIFFERENT email — the reported "same account, another machine" case.
/// `status` must classify it as yours, using the PR author login, not the email.
#[test]
fn test_status_recognizes_your_pr_committed_under_another_email() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }
    if !common::jj_available() {
        println!("Skipping E2E test (jj not available)");
        return;
    }

    let ctx = E2eContext::new();
    let name = ctx.bookmark_name("othermachine");
    let other_email = "e2e-other-machine@invalid.example";
    ctx.commit_as(other_email, &format!("{name}.rs"), "// other machine\n", "Work from another machine", &name);

    // Create the PR (opened by the authenticated user) using an identity that
    // includes the foreign email, so setup discovery finds the branch.
    let mut jj = ctx.runner();
    jj.set_identity(&Identity { emails: vec![other_email.to_string()], logins: vec![] });
    let github = github_forge();
    let repo_info = RepoInfo { owner: OWNER.to_string(), repo: REPO.to_string() };
    let graph = change_graph::build_change_graph(&jj).unwrap();
    let analysis = analyze::analyze_submission_graph(&graph, &name).unwrap();
    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, false).unwrap();
    let submission_plan = plan::create_submission_plan(
        &github, &segments, "origin", &repo_info, ForgeKind::GitHub, "main",
        &plan::SubmitOptions {
            draft_mode: plan::DraftMode::Default,
            reviewers: &[],
            reviewer_scope: jjpr::forge::types::ReviewerScope::Bottom,
            stack_base: None,
            stack_nav: jjpr::config::StackNavMode::Comment,
            dry_run: false,
        },
    )
    .unwrap();
    execute::execute_submission_plan(&jj, &github, &submission_plan).unwrap();
    assert!(find_pr(&name).is_some(), "PR should be created");

    // Now the display: the binary uses its OWN default identity (local email +
    // no config), so the commit is foreign by email. It must still read as yours
    // because the PR's author is your authenticated login.
    let output = Command::new(env!("CARGO_BIN_EXE_jjpr"))
        .args(["status", &name, "--no-fetch"])
        .current_dir(&ctx.repo_path)
        .output()
        .expect("run jjpr status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains(&name), "status should show the branch: {stdout}");
    assert!(!stdout.contains("someone else's"), "must not read as someone else's: {stdout}");
    assert!(!stdout.contains("won't submit or merge"), "must be actionable as yours: {stdout}");
    assert!(!stdout.contains(" by @"), "yours must not be attributed to another: {stdout}");
}

/// Tier 2 (auto-augmentation): `submit` recognizes a branch authored under a
/// VERIFIED account email that jjpr fetches from `/user/emails`. Needs the
/// token's `user` scope and a verified email distinct from the local one; skips
/// otherwise (that path degrades to the `[identity]` config backstop).
#[test]
fn test_submit_auto_fetches_verified_emails_for_other_machine_work() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }
    if !common::jj_available() {
        println!("Skipping E2E test (jj not available)");
        return;
    }

    let ctx = E2eContext::new();
    // Precondition, via the SAME token jjpr uses: a fetchable verified email
    // distinct from the local one. No such thing (missing `user` scope, or only
    // one verified email) → Tier 2 can't auto-recover here; skip.
    let verified = github_forge().get_authenticated_emails().unwrap_or_default();
    let local = ctx.local_email();
    let Some(other_email) = verified.into_iter().find(|e| *e != local) else {
        println!(
            "Skipping Tier 2 E2E: no verified account email distinct from local \
             (missing `user` scope, or a single verified email)"
        );
        return;
    };

    let name = ctx.bookmark_name("tier2");
    ctx.commit_as(&other_email, &format!("{name}.rs"), "// tier2\n", "Other-machine work", &name);

    // `submit --dry-run` with no bookmark: the seed (local email) can't see this
    // branch, but Tier 2 fetches the verified emails, recognizes it, and infers.
    let output = Command::new(env!("CARGO_BIN_EXE_jjpr"))
        .args(["submit", "--dry-run", "--no-fetch"])
        .current_dir(&ctx.repo_path)
        .output()
        .expect("run jjpr submit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&name),
        "Tier 2 should infer the other-email branch. stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("isn't recognized as yours"),
        "Tier 2 should have recognized it, not fallen back to the config hint: {stdout}"
    );
}

/// Config backstop: `[identity] emails` makes `submit` recognize a branch
/// authored under that email — no forge scope needed. Exercises the whole
/// config → seed → owned() → discovery path through the real binary, so it
/// runs even where Tier 2's `/user/emails` auto-fetch can't (as above).
#[test]
fn test_submit_recognizes_other_email_branch_via_identity_config() {
    if std::env::var("JJPR_E2E").is_err() {
        println!("Skipping E2E test (set JJPR_E2E=1 to run)");
        return;
    }
    if !common::jj_available() {
        println!("Skipping E2E test (jj not available)");
        return;
    }

    let ctx = E2eContext::new();
    let other_email = "e2e-config-backstop@invalid.example";
    let name = ctx.bookmark_name("configid");
    ctx.commit_as(other_email, &format!("{name}.rs"), "// config backstop\n", "Other-machine work", &name);

    // Declare the email as one of yours — no forge fetch involved.
    std::fs::write(
        ctx.repo_path.join(".jj").join("jjpr.toml"),
        format!("[identity]\nemails = [\"{other_email}\"]\n"),
    )
    .expect("write repo config");

    // `submit --dry-run` (no bookmark): the seed now covers the email, so
    // inference recognizes the branch without any network augmentation.
    let output = Command::new(env!("CARGO_BIN_EXE_jjpr"))
        .args(["submit", "--dry-run", "--no-fetch"])
        .current_dir(&ctx.repo_path)
        .output()
        .expect("run jjpr submit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&name),
        "config identity should let submit infer the branch. stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("isn't recognized as yours"),
        "the branch should be recognized via [identity] config: {stdout}"
    );
}
