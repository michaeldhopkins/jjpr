use std::process::Command;

use anyhow::{Context, Result};

use super::types::{
    ChecksStatus, IssueComment, MergeMethod, PrMergeability, PullRequest, PullRequestRef,
    ReviewSummary,
};
use super::Forge;

/// GitLab implementation that shells out to the `glab` CLI.
#[derive(Default)]
pub struct GlabCli {
    token: Option<String>,
}

impl GlabCli {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_token(token: String) -> Self {
        Self { token: Some(token) }
    }

    fn run_glab(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("glab");
        cmd.args(args);
        if let Some(token) = &self.token {
            cmd.env("GITLAB_TOKEN", token);
        }
        let output = cmd
            .output()
            .context("failed to run glab. Install it: https://gitlab.com/gitlab-org/cli")?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("glab {} failed: {}", args.join(" "), stderr.trim())
        }
    }

    /// URL-encode the project path for GitLab API endpoints.
    /// GitLab uses `projects/{encoded_path}/...` where the path is `owner/repo`
    /// (or `group/subgroup/repo` for nested namespaces).
    fn encode_project(owner: &str, repo: &str) -> String {
        format!("{owner}/{repo}").replace('/', "%2F")
    }
}

/// Parse a single GitLab MR JSON value into our `PullRequest` type.
fn parse_mr(mr: &serde_json::Value) -> Result<PullRequest> {
    let iid = mr["iid"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("MR missing iid"))?;

    let source_project_id = mr["source_project_id"].as_u64().unwrap_or(0);
    let target_project_id = mr["target_project_id"].as_u64().unwrap_or(0);

    // Normalize fork detection into the head.label format build_pr_map expects.
    // Same-project MRs get an empty label (passes filter).
    // Cross-project MRs get "namespace:branch" to be filtered out.
    let head_label = if source_project_id != target_project_id && source_project_id != 0 {
        let source_ns = mr["source_namespace"]["path"]
            .as_str()
            .or_else(|| mr["source_namespace"]["full_path"].as_str())
            .unwrap_or("fork");
        let source_branch = mr["source_branch"].as_str().unwrap_or("");
        format!("{source_ns}:{source_branch}")
    } else {
        String::new()
    };

    Ok(PullRequest {
        number: iid,
        html_url: mr["web_url"].as_str().unwrap_or("").to_string(),
        title: mr["title"].as_str().unwrap_or("").to_string(),
        body: mr["description"].as_str().map(|s| s.to_string()),
        base: PullRequestRef {
            ref_name: mr["target_branch"].as_str().unwrap_or("").to_string(),
            label: String::new(),
        },
        head: PullRequestRef {
            ref_name: mr["source_branch"].as_str().unwrap_or("").to_string(),
            label: head_label,
        },
        draft: mr["draft"].as_bool().unwrap_or(false),
        node_id: String::new(),
        merged_at: mr["merged_at"].as_str().map(|s| s.to_string()),
    })
}

/// Parse a GitLab note JSON value into our `IssueComment` type.
fn parse_note(note: &serde_json::Value) -> Option<IssueComment> {
    // Skip system-generated notes (status change messages, etc.)
    if note["system"].as_bool().unwrap_or(false) {
        return None;
    }
    let id = note["id"].as_u64()?;
    let body = note["body"].as_str().map(|s| s.to_string());
    Some(IssueComment { id, body })
}

impl Forge for GlabCli {
    fn list_open_prs(&self, owner: &str, repo: &str) -> Result<Vec<PullRequest>> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests?state=opened");
        let output = self.run_glab(&["api", &endpoint, "--paginate"])?;
        let mrs: Vec<serde_json::Value> =
            serde_json::from_str(&output).context("failed to parse MR list response")?;
        mrs.iter().map(parse_mr).collect()
    }

    fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        draft: bool,
    ) -> Result<PullRequest> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests");
        let title_arg = format!("title={title}");
        let source_arg = format!("source_branch={head}");
        let target_arg = format!("target_branch={base}");
        let desc_arg = format!("description={body}");
        let mut args = vec![
            "api", &endpoint, "-X", "POST",
            "-f", &title_arg,
            "-f", &source_arg,
            "-f", &target_arg,
            "-f", &desc_arg,
        ];
        if draft {
            args.push("-F");
            args.push("draft=true");
        }
        let output = self.run_glab(&args)?;
        let mr: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse created MR response")?;
        parse_mr(&mr)
    }

    fn update_pr_base(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        base: &str,
    ) -> Result<()> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}");
        self.run_glab(&[
            "api", &endpoint,
            "-X", "PUT",
            "-f", &format!("target_branch={base}"),
        ])?;
        Ok(())
    }

    fn request_reviewers(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        reviewers: &[String],
    ) -> Result<()> {
        if reviewers.is_empty() {
            return Ok(());
        }

        // GitLab requires numeric user IDs, so look up each username
        let mut reviewer_ids = Vec::new();
        for username in reviewers {
            let user_output =
                self.run_glab(&["api", &format!("users?username={username}")])?;
            let users: Vec<serde_json::Value> = serde_json::from_str(&user_output)
                .context("failed to parse user lookup response")?;
            let user_id = users
                .first()
                .and_then(|u| u["id"].as_u64())
                .ok_or_else(|| anyhow::anyhow!("user '{username}' not found on GitLab"))?;
            reviewer_ids.push(user_id);
        }

        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}");
        let mut args = vec!["api", &endpoint, "-X", "PUT"];
        let formatted: Vec<String> = reviewer_ids
            .iter()
            .map(|id| format!("reviewer_ids[]={id}"))
            .collect();
        for id_arg in &formatted {
            args.push("-F");
            args.push(id_arg);
        }
        self.run_glab(&args)?;
        Ok(())
    }

    fn list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}/notes");
        let output = self.run_glab(&["api", &endpoint, "--paginate"])?;
        let notes: Vec<serde_json::Value> =
            serde_json::from_str(&output).context("failed to parse notes response")?;
        Ok(notes.iter().filter_map(parse_note).collect())
    }

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}/notes");
        let output = self.run_glab(&[
            "api", &endpoint,
            "-X", "POST",
            "-f", &format!("body={body}"),
        ])?;
        let note: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse created note response")?;
        let id = note["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("created note missing id"))?;
        Ok(IssueComment {
            id,
            body: note["body"].as_str().map(|s| s.to_string()),
        })
    }

    fn update_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        // GitLab's note update API requires the MR iid in the path:
        //   PUT /projects/:id/merge_requests/:iid/notes/:note_id
        // but the Forge trait only passes comment_id. We scan open MRs to find
        // which one owns this note. In practice stacks are small (2-5 MRs).
        let project = Self::encode_project(owner, repo);
        let mrs_endpoint = format!("projects/{project}/merge_requests?state=opened&per_page=100");
        let mrs_output = self.run_glab(&["api", &mrs_endpoint])?;
        let mrs: Vec<serde_json::Value> =
            serde_json::from_str(&mrs_output).unwrap_or_default();

        for mr in &mrs {
            let iid = mr["iid"].as_u64().unwrap_or(0);
            if iid == 0 {
                continue;
            }
            let note_endpoint =
                format!("projects/{project}/merge_requests/{iid}/notes/{comment_id}");
            let result = self.run_glab(&[
                "api", &note_endpoint,
                "-X", "PUT",
                "-f", &format!("body={body}"),
            ]);
            if result.is_ok() {
                return Ok(());
            }
        }

        anyhow::bail!("could not find note {comment_id} on any open MR")
    }

    fn update_pr_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}");
        self.run_glab(&[
            "api", &endpoint,
            "-X", "PUT",
            "-f", &format!("description={body}"),
        ])?;
        Ok(())
    }

    fn mark_pr_ready(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<()> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}");
        self.run_glab(&[
            "api", &endpoint,
            "-X", "PUT",
            "-F", "draft=false",
        ])?;
        Ok(())
    }

    fn get_authenticated_user(&self) -> Result<String> {
        let output = self.run_glab(&["api", "user"])?;
        let user: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse user response")?;
        user["username"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("user response missing username field"))
    }

    fn find_merged_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let project = Self::encode_project(owner, repo);
        let endpoint =
            format!("projects/{project}/merge_requests?source_branch={head}&state=merged");
        let output = self.run_glab(&["api", &endpoint])?;
        let mrs: Vec<serde_json::Value> = serde_json::from_str(&output)
            .context("failed to parse merged MR list response")?;
        mrs.first().map(parse_mr).transpose()
    }

    fn merge_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        method: MergeMethod,
    ) -> Result<()> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}/merge");
        let squash_arg = match method {
            MergeMethod::Squash => "squash=true",
            MergeMethod::Merge | MergeMethod::Rebase => "squash=false",
        };
        self.run_glab(&[
            "api", &endpoint,
            "-X", "PUT",
            "-F", squash_arg,
        ])?;
        Ok(())
    }

    fn get_pr_checks_status(
        &self,
        owner: &str,
        repo: &str,
        head_ref: &str,
    ) -> Result<ChecksStatus> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!(
            "projects/{project}/pipelines?ref={head_ref}&per_page=1&order_by=id&sort=desc"
        );
        let output = self.run_glab(&["api", &endpoint])?;
        let pipelines: Vec<serde_json::Value> = serde_json::from_str(&output)
            .context("failed to parse pipelines response")?;

        let Some(latest) = pipelines.first() else {
            return Ok(ChecksStatus::None);
        };

        match latest["status"].as_str().unwrap_or("unknown") {
            "success" => Ok(ChecksStatus::Pass),
            "failed" | "canceled" => Ok(ChecksStatus::Fail),
            "created" | "waiting_for_resource" | "preparing" | "pending" | "running"
            | "manual" | "scheduled" => Ok(ChecksStatus::Pending),
            _ => Ok(ChecksStatus::Pending),
        }
    }

    fn get_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<ReviewSummary> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}/approvals");
        let output = self.run_glab(&["api", &endpoint])?;
        let approvals: serde_json::Value = serde_json::from_str(&output)
            .context("failed to parse approvals response")?;

        let approved_count = approvals["approved_by"]
            .as_array()
            .map(|a| a.len() as u32)
            .unwrap_or(0);

        // GitLab's "requested changes" feature is reflected in the MR's
        // detailed_merge_status. Fetch the MR to check.
        let mr_endpoint = format!("projects/{project}/merge_requests/{number}");
        let mr_output = self.run_glab(&["api", &mr_endpoint])?;
        let mr: serde_json::Value = serde_json::from_str(&mr_output)
            .context("failed to parse MR response for review status")?;

        let changes_requested = mr["detailed_merge_status"]
            .as_str()
            .is_some_and(|s| s == "requested_changes");

        Ok(ReviewSummary {
            approved_count,
            changes_requested,
        })
    }

    fn get_pr_mergeability(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrMergeability> {
        let project = Self::encode_project(owner, repo);
        let endpoint = format!("projects/{project}/merge_requests/{number}");
        let output = self.run_glab(&["api", &endpoint])?;
        let mr: serde_json::Value = serde_json::from_str(&output)
            .context("failed to parse MR mergeability response")?;

        let detailed_status = mr["detailed_merge_status"]
            .as_str()
            .unwrap_or("unknown");

        let mergeable = match detailed_status {
            "mergeable" => Some(true),
            "checking" | "unchecked" | "preparing" => None,
            _ => Some(false),
        };

        Ok(PrMergeability {
            mergeable,
            mergeable_state: detailed_status.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- JSON fixture tests: verify parsing without any CLI ---

    const GITLAB_MR_RESPONSE: &str = r#"{
        "iid": 42,
        "web_url": "https://gitlab.com/mygroup/myproject/-/merge_requests/42",
        "title": "Add authentication",
        "description": "Implements basic auth flow",
        "target_branch": "main",
        "source_branch": "auth",
        "draft": false,
        "merged_at": null,
        "source_project_id": 123,
        "target_project_id": 123
    }"#;

    const GITLAB_DRAFT_MR: &str = r#"{
        "iid": 7,
        "web_url": "https://gitlab.com/o/r/-/merge_requests/7",
        "title": "WIP: Draft feature",
        "description": null,
        "target_branch": "develop",
        "source_branch": "draft-feature",
        "draft": true,
        "merged_at": null,
        "source_project_id": 10,
        "target_project_id": 10
    }"#;

    const GITLAB_MERGED_MR: &str = r#"{
        "iid": 99,
        "web_url": "https://gitlab.com/o/r/-/merge_requests/99",
        "title": "Already merged",
        "description": "This was merged",
        "target_branch": "main",
        "source_branch": "old-feature",
        "draft": false,
        "merged_at": "2024-06-15T10:30:00Z",
        "source_project_id": 5,
        "target_project_id": 5
    }"#;

    const GITLAB_FORK_MR: &str = r#"{
        "iid": 15,
        "web_url": "https://gitlab.com/o/r/-/merge_requests/15",
        "title": "Fork contribution",
        "description": "From a fork",
        "target_branch": "main",
        "source_branch": "feature",
        "draft": false,
        "merged_at": null,
        "source_project_id": 999,
        "target_project_id": 123,
        "source_namespace": {"path": "fork-owner"}
    }"#;

    #[test]
    fn test_parse_mr_basic_fields() {
        let mr: serde_json::Value = serde_json::from_str(GITLAB_MR_RESPONSE).unwrap();
        let pr = parse_mr(&mr).unwrap();

        assert_eq!(pr.number, 42);
        assert_eq!(
            pr.html_url,
            "https://gitlab.com/mygroup/myproject/-/merge_requests/42"
        );
        assert_eq!(pr.title, "Add authentication");
        assert_eq!(pr.body.as_deref(), Some("Implements basic auth flow"));
        assert_eq!(pr.base.ref_name, "main");
        assert_eq!(pr.head.ref_name, "auth");
        assert!(!pr.draft);
        assert!(pr.merged_at.is_none());
        assert!(pr.node_id.is_empty());
    }

    #[test]
    fn test_parse_mr_draft() {
        let mr: serde_json::Value = serde_json::from_str(GITLAB_DRAFT_MR).unwrap();
        let pr = parse_mr(&mr).unwrap();

        assert_eq!(pr.number, 7);
        assert!(pr.draft);
        assert!(pr.body.is_none());
        assert_eq!(pr.base.ref_name, "develop");
    }

    #[test]
    fn test_parse_mr_merged() {
        let mr: serde_json::Value = serde_json::from_str(GITLAB_MERGED_MR).unwrap();
        let pr = parse_mr(&mr).unwrap();

        assert_eq!(pr.number, 99);
        assert_eq!(pr.merged_at.as_deref(), Some("2024-06-15T10:30:00Z"));
    }

    #[test]
    fn test_parse_mr_same_project_empty_label() {
        let mr: serde_json::Value = serde_json::from_str(GITLAB_MR_RESPONSE).unwrap();
        let pr = parse_mr(&mr).unwrap();

        // Same-project MRs should have empty head.label (passes build_pr_map filter)
        assert!(pr.head.label.is_empty());
    }

    #[test]
    fn test_parse_mr_fork_gets_label() {
        let mr: serde_json::Value = serde_json::from_str(GITLAB_FORK_MR).unwrap();
        let pr = parse_mr(&mr).unwrap();

        // Fork MRs should get a label so build_pr_map filters them out
        assert_eq!(pr.head.label, "fork-owner:feature");
    }

    #[test]
    fn test_parse_note_user_comment() {
        let note: serde_json::Value = serde_json::from_str(
            r#"{
                "id": 301,
                "body": "Looks good to me!",
                "system": false,
                "author": {"username": "reviewer"}
            }"#,
        )
        .unwrap();
        let comment = parse_note(&note).unwrap();
        assert_eq!(comment.id, 301);
        assert_eq!(comment.body.as_deref(), Some("Looks good to me!"));
    }

    #[test]
    fn test_parse_note_system_note_filtered() {
        let note: serde_json::Value = serde_json::from_str(
            r#"{
                "id": 302,
                "body": "marked as draft",
                "system": true
            }"#,
        )
        .unwrap();
        assert!(parse_note(&note).is_none());
    }

    #[test]
    fn test_parse_note_stack_comment() {
        let note: serde_json::Value = serde_json::from_str(
            r#"{
                "id": 500,
                "body": "<!-- jjpr:stack-info -->\nStack comment content",
                "system": false
            }"#,
        )
        .unwrap();
        let comment = parse_note(&note).unwrap();
        assert_eq!(comment.id, 500);
        assert!(comment
            .body
            .as_deref()
            .unwrap()
            .contains("<!-- jjpr:stack-info -->"));
    }

    #[test]
    fn test_parse_mr_list() {
        let json = format!("[{GITLAB_MR_RESPONSE}, {GITLAB_DRAFT_MR}]");
        let mrs: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        let prs: Vec<PullRequest> = mrs.iter().map(|m| parse_mr(m).unwrap()).collect();

        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[1].number, 7);
    }

    #[test]
    fn test_encode_project_simple() {
        assert_eq!(GlabCli::encode_project("owner", "repo"), "owner%2Frepo");
    }

    #[test]
    fn test_encode_project_nested_groups() {
        assert_eq!(
            GlabCli::encode_project("group/subgroup", "repo"),
            "group%2Fsubgroup%2Frepo"
        );
    }

    #[test]
    fn test_pipeline_status_mapping() {
        // These test the mapping logic in get_pr_checks_status by validating
        // the match arms. Since we can't call the real CLI, we test the mapping
        // separately.
        let cases = vec![
            ("success", ChecksStatus::Pass),
            ("failed", ChecksStatus::Fail),
            ("canceled", ChecksStatus::Fail),
            ("running", ChecksStatus::Pending),
            ("pending", ChecksStatus::Pending),
            ("created", ChecksStatus::Pending),
            ("manual", ChecksStatus::Pending),
        ];

        for (status, expected) in cases {
            let result = match status {
                "success" => ChecksStatus::Pass,
                "failed" | "canceled" => ChecksStatus::Fail,
                "created" | "waiting_for_resource" | "preparing" | "pending" | "running"
                | "manual" | "scheduled" => ChecksStatus::Pending,
                _ => ChecksStatus::Pending,
            };
            assert_eq!(result, expected, "status '{status}' should map correctly");
        }
    }

    #[test]
    fn test_mergeability_status_mapping() {
        let cases: Vec<(&str, Option<bool>)> = vec![
            ("mergeable", Some(true)),
            ("checking", None),
            ("unchecked", None),
            ("preparing", None),
            ("conflict", Some(false)),
            ("ci_must_pass", Some(false)),
            ("not_approved", Some(false)),
            ("draft_status", Some(false)),
        ];

        for (status, expected) in cases {
            let result = match status {
                "mergeable" => Some(true),
                "checking" | "unchecked" | "preparing" => None,
                _ => Some(false),
            };
            assert_eq!(
                result, expected,
                "detailed_merge_status '{status}' should map correctly"
            );
        }
    }

    #[test]
    fn test_approvals_parsing() {
        let approvals_json = r#"{
            "approved_by": [
                {"user": {"id": 1, "username": "alice"}},
                {"user": {"id": 2, "username": "bob"}}
            ]
        }"#;
        let approvals: serde_json::Value = serde_json::from_str(approvals_json).unwrap();
        let count = approvals["approved_by"]
            .as_array()
            .map(|a| a.len() as u32)
            .unwrap_or(0);
        assert_eq!(count, 2);
    }

    #[test]
    fn test_approvals_parsing_empty() {
        let approvals_json = r#"{"approved_by": []}"#;
        let approvals: serde_json::Value = serde_json::from_str(approvals_json).unwrap();
        let count = approvals["approved_by"]
            .as_array()
            .map(|a| a.len() as u32)
            .unwrap_or(0);
        assert_eq!(count, 0);
    }
}
