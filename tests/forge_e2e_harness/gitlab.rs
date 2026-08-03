//! GitLab driver, via the `glab` CLI.
//!
//! Note: GitLab's "reset approvals on push" is a **project-level** setting, not
//! per-branch. `set_dismiss_stale`/`remove_protection` therefore toggle the
//! whole project's setting transiently (set → assert → reset within one test),
//! which is the only option GitLab offers. Keep that window short.

use std::process::Command;

use serde_json::Value;

use super::{ForgeTestDriver, MergeMethod, OWNER, REPO};

pub struct GitLabDriver;

pub fn available() -> bool {
    super::tool_available("glab")
        && Command::new("glab")
            .args(["auth", "status"])
            .output()
            .is_ok_and(|o| o.status.success())
}

fn enc() -> String {
    format!("{OWNER}%2F{REPO}")
}

// Run glab from a neutral dir so it never picks up the *current* repo's remotes
// (we run inside the jjpr checkout, whose remote is github.com). All calls use
// `glab api` with an explicit project path, so only the authed host + token matter.
fn glab(args: &[&str]) -> String {
    let out = Command::new("glab")
        .current_dir(std::env::temp_dir())
        .args(args)
        .output()
        .expect("run glab");
    assert!(
        out.status.success(),
        "glab {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn glab_quiet(args: &[&str]) {
    let _ = Command::new("glab")
        .current_dir(std::env::temp_dir())
        .args(args)
        .output();
}
fn api(path: &str) -> Value {
    serde_json::from_str(&glab(&["api", path])).unwrap_or(Value::Null)
}

impl ForgeTestDriver for GitLabDriver {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn clone_url(&self) -> String {
        format!("git@gitlab.com:{OWNER}/{REPO}.git")
    }

    fn open_request(&self, head: &str, base: &str, title: &str) -> u64 {
        let v: Value = serde_json::from_str(&glab(&[
            "api",
            "-X",
            "POST",
            &format!("projects/{}/merge_requests", enc()),
            "-f",
            &format!("source_branch={head}"),
            "-f",
            &format!("target_branch={base}"),
            "-f",
            &format!("title={title}"),
            "-f",
            "description=e2e fixture",
        ]))
        .unwrap_or(Value::Null);
        v["iid"]
            .as_u64()
            .unwrap_or_else(|| panic!("create MR failed: {v}"))
    }

    fn find_request_by_head(&self, head: &str) -> Option<u64> {
        let mrs = api(&format!(
            "projects/{}/merge_requests?source_branch={head}&state=all",
            enc()
        ));
        mrs.as_array()?.first()?.get("iid")?.as_u64()
    }

    fn request_head_sha(&self, number: u64) -> String {
        api(&format!("projects/{}/merge_requests/{number}", enc()))["sha"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn request_base(&self, number: u64) -> String {
        api(&format!("projects/{}/merge_requests/{number}", enc()))["target_branch"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn boxed(&self) -> Box<dyn ForgeTestDriver> {
        Box::new(GitLabDriver)
    }

    fn make_draft(&self, number: u64) {
        // GitLab's `draft` is derived from the title, not a settable field.
        let path = format!("projects/{}/merge_requests/{number}", enc());
        let title = api(&path)["title"].as_str().unwrap_or("").to_string();
        glab(&[
            "api",
            "-X",
            "PUT",
            &path,
            "-f",
            &format!("title=Draft: {title}"),
        ]);
    }

    fn request_state(&self, number: u64) -> String {
        match api(&format!("projects/{}/merge_requests/{number}", enc()))["state"]
            .as_str()
            .unwrap_or("")
        {
            "opened" => "open".to_string(),
            other => other.to_string(),
        }
    }

    fn admin_merge(&self, number: u64, method: MergeMethod) {
        let path = format!("projects/{}/merge_requests/{number}/merge", enc());
        let mut args: Vec<&str> = vec!["api", "-X", "PUT", &path];
        if method == MergeMethod::Squash {
            args.push("-f");
            args.push("squash=true");
        }
        glab(&args);
    }

    fn dismiss_stale_toggle_supported(&self) -> bool {
        // reset_approvals_on_push is GitLab Premium; a free sandbox silently
        // ignores the write, so we can't build the "on" precondition here.
        false
    }

    fn squash_rewrites_history(&self) -> bool {
        // A single-commit squash fast-forwards the original commit unchanged on
        // the sandbox project, so the "rebase happens" control can't rely on it.
        false
    }

    fn set_dismiss_stale(&self, _branch: &str) {
        // Project-level; branch is irrelevant.
        glab_quiet(&[
            "api",
            "-X",
            "POST",
            &format!("projects/{}/approvals", enc()),
            "-f",
            "reset_approvals_on_push=true",
        ]);
    }

    fn remove_protection(&self, _branch: &str) {
        glab_quiet(&[
            "api",
            "-X",
            "POST",
            &format!("projects/{}/approvals", enc()),
            "-f",
            "reset_approvals_on_push=false",
        ]);
    }

    fn jjpr_forge(&self) -> Box<dyn jjpr::forge::Forge> {
        use jjpr::forge::{AuthScheme, ForgeClient, ForgeKind, GitLabForge, PaginationStyle};
        let token =
            jjpr::forge::token::resolve_token(ForgeKind::GitLab, None).expect("gitlab token");
        let client = ForgeClient::new(
            "https://gitlab.com/api/v4",
            token,
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        Box::new(GitLabForge::new(client))
    }

    fn cleanup_prefix(&self, prefix: &str) {
        // Close prefixed open MRs.
        let mrs = api(&format!(
            "projects/{}/merge_requests?state=opened&per_page=100",
            enc()
        ));
        if let Some(arr) = mrs.as_array() {
            for mr in arr {
                if mr["source_branch"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with(prefix)
                    && let Some(iid) = mr["iid"].as_u64()
                {
                    glab_quiet(&[
                        "api",
                        "-X",
                        "PUT",
                        &format!("projects/{}/merge_requests/{iid}?state_event=close", enc()),
                    ]);
                }
            }
        }
        // Delete prefixed branches.
        let branches = api(&format!(
            "projects/{}/repository/branches?per_page=100&search={prefix}",
            enc()
        ));
        if let Some(arr) = branches.as_array() {
            for b in arr {
                if let Some(name) = b["name"].as_str()
                    && name.starts_with(prefix)
                {
                    // Prefixed branch names are alphanumeric + hyphens — no encoding needed.
                    glab_quiet(&[
                        "api",
                        "-X",
                        "DELETE",
                        &format!("projects/{}/repository/branches/{name}", enc()),
                    ]);
                }
            }
        }
        // Reset the project approval setting (best effort).
        self.remove_protection("");
    }
}
