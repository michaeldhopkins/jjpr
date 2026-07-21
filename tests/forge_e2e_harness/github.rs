//! GitHub driver, via the `gh` CLI.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

use super::{ForgeTestDriver, MergeMethod, OWNER, REPO};

pub struct GitHubDriver;

pub fn available() -> bool {
    super::tool_available("gh")
        && Command::new("gh").args(["auth", "status"]).output().is_ok_and(|o| o.status.success())
}

fn repo_slug() -> String {
    format!("{OWNER}/{REPO}")
}

/// Run `gh` and require success, returning stdout.
fn gh(args: &[&str]) -> String {
    let out = Command::new("gh").args(args).output().expect("run gh");
    assert!(out.status.success(), "gh {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run `gh`, ignoring failure (best-effort cleanup).
fn gh_quiet(args: &[&str]) {
    let _ = Command::new("gh").args(args).output();
}

/// `gh api` POST/PUT with a JSON body piped on stdin.
fn gh_api_input(method: &str, path: &str, body: &Value) -> String {
    let mut child = Command::new("gh")
        .args(["api", "-X", method, path, "--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gh api");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body.to_string().as_bytes())
        .expect("write body");
    let out = child.wait_with_output().expect("gh api");
    assert!(out.status.success(), "gh api {method} {path} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ruleset_name(branch: &str) -> String {
    format!("ds-{branch}")
}

impl ForgeTestDriver for GitHubDriver {
    fn name(&self) -> &'static str {
        "github"
    }

    fn clone_url(&self) -> String {
        format!("git@github.com:{OWNER}/{REPO}.git")
    }

    fn open_request(&self, head: &str, base: &str, title: &str) -> u64 {
        gh(&[
            "pr", "create", "--repo", &repo_slug(),
            "--head", head, "--base", base,
            "--title", title, "--body", "e2e fixture",
        ]);
        self.find_request_by_head(head).expect("PR just created")
    }

    fn find_request_by_head(&self, head: &str) -> Option<u64> {
        let out = gh(&[
            "pr", "list", "--repo", &repo_slug(), "--head", head,
            "--state", "all", "--json", "number",
        ]);
        let prs: Vec<Value> = serde_json::from_str(&out).ok()?;
        prs.first()?.get("number")?.as_u64()
    }

    fn request_head_sha(&self, number: u64) -> String {
        gh(&["pr", "view", &number.to_string(), "--repo", &repo_slug(), "--json", "headRefOid", "-q", ".headRefOid"])
            .trim()
            .to_string()
    }

    fn request_base(&self, number: u64) -> String {
        gh(&["pr", "view", &number.to_string(), "--repo", &repo_slug(), "--json", "baseRefName", "-q", ".baseRefName"])
            .trim()
            .to_string()
    }

    fn boxed(&self) -> Box<dyn ForgeTestDriver> {
        Box::new(GitHubDriver)
    }

    fn make_draft(&self, number: u64) {
        gh(&["pr", "ready", &number.to_string(), "--undo", "--repo", &repo_slug()]);
    }

    fn request_state(&self, number: u64) -> String {
        gh(&["pr", "view", &number.to_string(), "--repo", &repo_slug(), "--json", "state", "-q", ".state"])
            .trim()
            .to_lowercase()
    }

    fn admin_merge(&self, number: u64, method: MergeMethod) {
        let flag = match method {
            MergeMethod::MergeCommit => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        };
        gh(&["pr", "merge", &number.to_string(), "--repo", &repo_slug(), flag, "--admin"]);
    }

    fn set_dismiss_stale(&self, branch: &str) {
        let body = serde_json::json!({
            "name": ruleset_name(branch),
            "target": "branch",
            "enforcement": "active",
            "conditions": {"ref_name": {"include": [format!("refs/heads/{branch}")], "exclude": []}},
            "rules": [{"type": "pull_request", "parameters": {
                "required_approving_review_count": 1,
                "dismiss_stale_reviews_on_push": true,
                "require_code_owner_review": false,
                "require_last_push_approval": false,
                "required_review_thread_resolution": false
            }}]
        });
        gh_api_input("POST", &format!("repos/{}/rulesets", repo_slug()), &body);
    }

    fn remove_protection(&self, branch: &str) {
        delete_rulesets_where(|name| name == ruleset_name(branch));
    }

    fn jjpr_forge(&self) -> Box<dyn jjpr::forge::Forge> {
        use jjpr::forge::{AuthScheme, ForgeClient, ForgeKind, GitHubForge, PaginationStyle};
        let token = jjpr::forge::token::resolve_token(ForgeKind::GitHub, None).expect("github token");
        let client = ForgeClient::new("https://api.github.com", token, AuthScheme::Bearer, PaginationStyle::LinkHeader);
        Box::new(GitHubForge::new(client))
    }

    fn cleanup_prefix(&self, prefix: &str) {
        let slug = repo_slug();
        // Close prefixed open PRs.
        if let Ok(prs) = serde_json::from_str::<Vec<Value>>(&gh(&[
            "pr", "list", "--repo", &slug, "--json", "number,headRefName",
            "--state", "open", "--limit", "50",
        ])) {
            for pr in &prs {
                if pr["headRefName"].as_str().unwrap_or("").starts_with(prefix)
                    && let Some(n) = pr["number"].as_u64()
                {
                    gh_quiet(&["pr", "close", &n.to_string(), "--repo", &slug]);
                }
            }
        }
        // Delete prefixed refs.
        if let Ok(refs) = serde_json::from_str::<Vec<Value>>(&gh(&[
            "api", &format!("repos/{slug}/git/matching-refs/heads/{prefix}"),
        ])) {
            for r in &refs {
                if let Some(ref_name) = r["ref"].as_str() {
                    gh_quiet(&["api", &format!("repos/{slug}/git/{ref_name}"), "-X", "DELETE"]);
                }
            }
        }
        // Delete any rulesets this run created (named ds-{prefix}...).
        delete_rulesets_where(|name| name.starts_with(&format!("ds-{prefix}")));
    }
}

/// List repo rulesets and delete those whose name matches `pred`.
fn delete_rulesets_where(pred: impl Fn(&str) -> bool) {
    let slug = repo_slug();
    let Ok(rulesets) = serde_json::from_str::<Vec<Value>>(&gh(&["api", &format!("repos/{slug}/rulesets")])) else {
        return;
    };
    for rs in &rulesets {
        if rs["name"].as_str().is_some_and(&pred)
            && let Some(id) = rs["id"].as_u64()
        {
            gh_quiet(&["api", &format!("repos/{slug}/rulesets/{id}"), "-X", "DELETE"]);
        }
    }
}
