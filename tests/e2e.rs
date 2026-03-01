mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use jjpr::forge::{AuthScheme, ForgeClient, ForgeKind, GitHubForge, PaginationStyle};
use jjpr::forge::types::RepoInfo;
use jjpr::graph::change_graph;
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
        &github, &segments, "origin", &repo_info, ForgeKind::GitHub, "main", false, false, &[], None,
    )
    .unwrap();

    assert_eq!(submission_plan.bookmarks_needing_push.len(), 2);
    assert_eq!(submission_plan.bookmarks_needing_pr.len(), 2);
    assert_eq!(submission_plan.bookmarks_needing_pr[0].base_branch, "main");
    assert_eq!(
        submission_plan.bookmarks_needing_pr[1].base_branch,
        auth_name
    );

    execute::execute_submission_plan(
        &jj, &github, &submission_plan, &[], false,
    )
    .unwrap();

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
