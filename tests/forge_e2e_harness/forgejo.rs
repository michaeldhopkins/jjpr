//! Forgejo/Codeberg driver. No CLI — REST via `curl` + `FORGEJO_TOKEN`.

use std::process::Command;

use serde_json::{Value, json};

use super::{ForgeTestDriver, MergeMethod, OWNER, REPO};

pub struct ForgejoDriver;

pub fn available() -> bool {
    std::env::var("FORGEJO_TOKEN").is_ok_and(|t| !t.is_empty()) && super::tool_available("curl")
}

const HOST: &str = "https://codeberg.org/api/v1";

/// One REST call. Returns the parsed JSON body (or Null on empty/non-JSON).
fn req(method: &str, path: &str, body: Option<Value>) -> Value {
    let token = std::env::var("FORGEJO_TOKEN").expect("FORGEJO_TOKEN");
    let mut args: Vec<String> = vec![
        "-s".into(),
        "-X".into(),
        method.into(),
        "-H".into(),
        format!("Authorization: token {token}"),
        "-H".into(),
        "Content-Type: application/json".into(),
        format!("{HOST}/{path}"),
    ];
    if let Some(b) = body {
        args.push("-d".into());
        args.push(b.to_string());
    }
    let out = Command::new("curl").args(&args).output().expect("curl");
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn repo_path(rest: &str) -> String {
    format!("repos/{OWNER}/{REPO}/{rest}")
}

impl ForgeTestDriver for ForgejoDriver {
    fn name(&self) -> &'static str {
        "forgejo"
    }

    fn clone_url(&self) -> String {
        // Codeberg SSH isn't configured here; clone over HTTPS with the token.
        // The URL lands in the throwaway temp clone's config only.
        let token = std::env::var("FORGEJO_TOKEN").unwrap_or_default();
        format!("https://{OWNER}:{token}@codeberg.org/{OWNER}/{REPO}.git")
    }

    fn open_request(&self, head: &str, base: &str, title: &str) -> u64 {
        let v = req(
            "POST",
            &repo_path("pulls"),
            Some(json!({
                "head": head, "base": base, "title": title, "body": "e2e fixture",
            })),
        );
        v["number"]
            .as_u64()
            .unwrap_or_else(|| panic!("create PR failed: {v}"))
    }

    fn find_request_by_head(&self, head: &str) -> Option<u64> {
        let v = req("GET", &repo_path("pulls?state=all&limit=50"), None);
        v.as_array()?
            .iter()
            .find(|pr| pr["head"]["ref"].as_str() == Some(head))?
            .get("number")?
            .as_u64()
    }

    fn request_head_sha(&self, number: u64) -> String {
        req("GET", &repo_path(&format!("pulls/{number}")), None)["head"]["sha"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn request_base(&self, number: u64) -> String {
        req("GET", &repo_path(&format!("pulls/{number}")), None)["base"]["ref"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn boxed(&self) -> Box<dyn ForgeTestDriver> {
        Box::new(ForgejoDriver)
    }

    fn squash_rewrites_history(&self) -> bool {
        // Like GitLab, a single-commit squash can fast-forward unchanged here.
        false
    }

    fn make_draft(&self, number: u64) {
        // Forgejo derives `draft` from a WIP title prefix.
        let title = req("GET", &repo_path(&format!("pulls/{number}")), None)["title"]
            .as_str()
            .unwrap_or("")
            .to_string();
        req(
            "PATCH",
            &repo_path(&format!("pulls/{number}")),
            Some(json!({ "title": format!("WIP: {title}") })),
        );
    }

    fn request_state(&self, number: u64) -> String {
        let pr = req("GET", &repo_path(&format!("pulls/{number}")), None);
        if pr["merged"].as_bool() == Some(true) {
            "merged".to_string()
        } else {
            pr["state"].as_str().unwrap_or("").to_string()
        }
    }

    fn admin_merge(&self, number: u64, method: MergeMethod) {
        let do_ = match method {
            MergeMethod::MergeCommit => "merge",
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
        };
        req(
            "POST",
            &repo_path(&format!("pulls/{number}/merge")),
            Some(json!({ "Do": do_ })),
        );
    }

    fn set_dismiss_stale(&self, branch: &str) {
        req(
            "POST",
            &repo_path("branch_protections"),
            Some(json!({
                "branch_name": branch,
                "dismiss_stale_approvals": true,
                "enable_approvals_whitelist": false,
                "required_approvals": 1,
            })),
        );
    }

    fn remove_protection(&self, branch: &str) {
        req(
            "DELETE",
            &repo_path(&format!("branch_protections/{branch}")),
            None,
        );
    }

    fn jjpr_forge(&self) -> Box<dyn jjpr::forge::Forge> {
        use jjpr::forge::{AuthScheme, ForgeClient, ForgejoForge, PaginationStyle};
        let token = std::env::var("FORGEJO_TOKEN").expect("FORGEJO_TOKEN");
        let client = ForgeClient::new(
            "https://codeberg.org/api/v1",
            token,
            AuthScheme::Token,
            PaginationStyle::PageNumber { limit: 50 },
        );
        Box::new(ForgejoForge::new(client))
    }

    fn cleanup_prefix(&self, prefix: &str) {
        // Close prefixed open PRs.
        if let Some(prs) = req("GET", &repo_path("pulls?state=open&limit=50"), None).as_array() {
            for pr in prs {
                if pr["head"]["ref"].as_str().unwrap_or("").starts_with(prefix)
                    && let Some(n) = pr["number"].as_u64()
                {
                    req(
                        "PATCH",
                        &repo_path(&format!("pulls/{n}")),
                        Some(json!({"state": "closed"})),
                    );
                }
            }
        }
        // Delete prefixed branch protections.
        if let Some(bps) = req("GET", &repo_path("branch_protections"), None).as_array() {
            for bp in bps {
                if let Some(name) = bp["branch_name"].as_str()
                    && name.starts_with(prefix)
                {
                    req(
                        "DELETE",
                        &repo_path(&format!("branch_protections/{name}")),
                        None,
                    );
                }
            }
        }
        // Delete prefixed branches.
        if let Some(branches) = req("GET", &repo_path("branches?limit=50"), None).as_array() {
            for b in branches {
                if let Some(name) = b["name"].as_str()
                    && name.starts_with(prefix)
                {
                    req("DELETE", &repo_path(&format!("branches/{name}")), None);
                }
            }
        }
    }
}
