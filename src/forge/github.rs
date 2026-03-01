use anyhow::{Context, Result};

use super::http::ForgeClient;
use super::types::{ChecksStatus, IssueComment, MergeMethod, PrMergeability, PullRequest, ReviewSummary};
use super::Forge;

/// GitHub implementation using direct HTTP via `ForgeClient`.
pub struct GitHubForge {
    client: ForgeClient,
}

impl GitHubForge {
    pub fn new(client: ForgeClient) -> Self {
        Self { client }
    }
}

/// Parse check-runs and commit status into a `ChecksStatus`.
fn parse_checks_status(
    check_runs: &serde_json::Value,
    commit_status: &serde_json::Value,
) -> ChecksStatus {
    let runs = check_runs["check_runs"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default();
    let statuses = commit_status["statuses"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default();

    if runs.is_empty() && statuses.is_empty() {
        return ChecksStatus::None;
    }

    let mut has_pending = false;
    let mut has_failure = false;

    for run in runs {
        match run["conclusion"].as_str() {
            Some("success") | Some("skipped") | Some("neutral") => {}
            None if run["status"].as_str() == Some("in_progress")
                || run["status"].as_str() == Some("queued") =>
            {
                has_pending = true;
            }
            _ => has_failure = true,
        }
    }

    for s in statuses {
        match s["state"].as_str() {
            Some("success") => {}
            Some("pending") => has_pending = true,
            _ => has_failure = true,
        }
    }

    if has_failure {
        ChecksStatus::Fail
    } else if has_pending {
        ChecksStatus::Pending
    } else {
        ChecksStatus::Pass
    }
}

/// Track each reviewer's latest meaningful review state.
/// COMMENTED and PENDING don't change approval status on GitHub,
/// so we skip them to avoid overwriting a valid APPROVED state.
fn parse_review_summary(reviews: &[serde_json::Value]) -> ReviewSummary {
    let mut latest: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for review in reviews {
        let user = review["user"]["login"].as_str().unwrap_or_default();
        let state = review["state"].as_str().unwrap_or_default();
        if !user.is_empty()
            && matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED")
        {
            latest.insert(user.to_string(), state.to_string());
        }
    }

    let approved_count = latest.values().filter(|s| *s == "APPROVED").count() as u32;
    let changes_requested = latest.values().any(|s| s == "CHANGES_REQUESTED");

    ReviewSummary {
        approved_count,
        changes_requested,
    }
}

impl Forge for GitHubForge {
    fn list_open_prs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<PullRequest>> {
        let path = format!("repos/{owner}/{repo}/pulls?state=open&per_page=100");
        let items = self.client.get_paginated(&path)?;
        serde_json::from_value(serde_json::Value::Array(items))
            .context("failed to parse PR list response")
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
        let path = format!("repos/{owner}/{repo}/pulls");
        let mut json_body = serde_json::json!({
            "title": title,
            "head": head,
            "base": base,
            "body": body,
        });
        if draft {
            json_body["draft"] = serde_json::json!(true);
        }
        let output = self.client.post(&path, &json_body)?;
        serde_json::from_value(output).context("failed to parse created PR response")
    }

    fn update_pr_base(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        base: &str,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        self.client.patch(&path, &serde_json::json!({ "base": base }))?;
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
        let path = format!("repos/{owner}/{repo}/pulls/{number}/requested_reviewers");
        self.client.post(&path, &serde_json::json!({ "reviewers": reviewers }))?;
        Ok(())
    }

    fn list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>> {
        let path = format!("repos/{owner}/{repo}/issues/{number}/comments?per_page=100");
        let items = self.client.get_paginated(&path)?;
        serde_json::from_value(serde_json::Value::Array(items))
            .context("failed to parse comments response")
    }

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let path = format!("repos/{owner}/{repo}/issues/{number}/comments");
        let output = self.client.post(&path, &serde_json::json!({ "body": body }))?;
        serde_json::from_value(output).context("failed to parse created comment response")
    }

    fn update_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/issues/comments/{comment_id}");
        self.client.patch(&path, &serde_json::json!({ "body": body }))?;
        Ok(())
    }

    fn update_pr_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        self.client.patch(&path, &serde_json::json!({ "body": body }))?;
        Ok(())
    }

    fn mark_pr_ready(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<()> {
        // GitHub requires GraphQL for marking a PR as ready.
        // First fetch the node_id from REST, then use it in the mutation.
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        let pr = self.client.get(&path)?;
        let node_id = pr["node_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("PR response missing node_id field"))?;

        let query = "mutation($id: ID!) { markPullRequestReadyForReview(input: { pullRequestId: $id }) { clientMutationId } }";
        self.client.graphql(
            "graphql",
            query,
            &serde_json::json!({ "id": node_id }),
        )?;
        Ok(())
    }

    fn find_merged_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let path = format!(
            "repos/{owner}/{repo}/pulls?head={owner}:{head}&state=closed"
        );
        let output = self.client.get(&path)?;
        let prs: Vec<PullRequest> = serde_json::from_value(output)
            .context("failed to parse closed PR list response")?;
        Ok(prs.into_iter().find(|pr| pr.merged_at.is_some()))
    }

    fn get_authenticated_user(&self) -> Result<String> {
        let output = self.client.get("user")?;
        output["login"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("user response missing login field"))
    }

    fn merge_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        method: MergeMethod,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}/merge");
        self.client.put(&path, &serde_json::json!({ "merge_method": method.to_string() }))?;
        Ok(())
    }

    fn get_pr_checks_status(
        &self,
        owner: &str,
        repo: &str,
        head_ref: &str,
    ) -> Result<ChecksStatus> {
        let check_runs_path =
            format!("repos/{owner}/{repo}/commits/{head_ref}/check-runs");
        let check_runs = self.client.get(&check_runs_path)?;

        let status_path =
            format!("repos/{owner}/{repo}/commits/{head_ref}/status");
        let status = self.client.get(&status_path)?;

        Ok(parse_checks_status(&check_runs, &status))
    }

    fn get_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<ReviewSummary> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100");
        let items = self.client.get_paginated(&path)?;
        Ok(parse_review_summary(&items))
    }

    fn get_pr_mergeability(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrMergeability> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        let pr = self.client.get(&path)?;

        let mergeable = pr["mergeable"].as_bool();
        let mergeable_state = pr["mergeable_state"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        Ok(PrMergeability {
            mergeable,
            mergeable_state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_checks_all_passing() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "success", "status": "completed"},
                {"conclusion": "skipped", "status": "completed"},
            ]
        });
        let status = serde_json::json!({
            "statuses": [
                {"state": "success"}
            ]
        });
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pass);
    }

    #[test]
    fn test_parse_checks_pending() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": null, "status": "in_progress"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pending);
    }

    #[test]
    fn test_parse_checks_failure() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "failure", "status": "completed"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Fail);
    }

    #[test]
    fn test_parse_checks_none() {
        let check_runs = serde_json::json!({"check_runs": []});
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::None);
    }

    #[test]
    fn test_parse_checks_mixed_failure_wins() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "success", "status": "completed"},
                {"conclusion": "failure", "status": "completed"},
            ]
        });
        let status = serde_json::json!({
            "statuses": [{"state": "pending"}]
        });
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Fail);
    }

    #[test]
    fn test_parse_checks_queued_is_pending() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": null, "status": "queued"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pending);
    }

    #[test]
    fn test_parse_checks_neutral_passes() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "neutral", "status": "completed"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pass);
    }

    #[test]
    fn test_review_latest_state_wins() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "CHANGES_REQUESTED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 0);
        assert!(summary.changes_requested);
    }

    #[test]
    fn test_review_commented_does_not_override() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "COMMENTED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 1);
        assert!(!summary.changes_requested);
    }

    #[test]
    fn test_review_pending_does_not_override() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "PENDING"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 1);
        assert!(!summary.changes_requested);
    }

    #[test]
    fn test_review_multiple_reviewers() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "bob"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "charlie"}, "state": "CHANGES_REQUESTED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 2);
        assert!(summary.changes_requested);
    }

    #[test]
    fn test_review_dismissed_clears_approval() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "DISMISSED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 0);
        assert!(!summary.changes_requested);
    }

    #[test]
    fn test_parse_mergeability_clean() {
        let pr = serde_json::json!({"mergeable": true, "mergeable_state": "clean"});
        assert_eq!(pr["mergeable"].as_bool(), Some(true));
        assert_eq!(pr["mergeable_state"].as_str(), Some("clean"));
    }

    #[test]
    fn test_parse_mergeability_dirty() {
        let pr = serde_json::json!({"mergeable": false, "mergeable_state": "dirty"});
        assert_eq!(pr["mergeable"].as_bool(), Some(false));
        assert_eq!(pr["mergeable_state"].as_str(), Some("dirty"));
    }

    #[test]
    fn test_parse_pr_basic_fields() {
        let json = r#"{
            "number": 42,
            "html_url": "https://github.com/owner/repo/pull/42",
            "title": "Add auth",
            "body": "Auth implementation",
            "base": {"ref": "main", "label": "owner:main"},
            "head": {"ref": "auth", "label": "owner:auth"},
            "draft": false,
            "node_id": "PR_kwDOABC123",
            "merged_at": null
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.html_url, "https://github.com/owner/repo/pull/42");
        assert_eq!(pr.title, "Add auth");
        assert_eq!(pr.base.ref_name, "main");
        assert_eq!(pr.head.ref_name, "auth");
        assert!(!pr.draft);
        assert_eq!(pr.node_id, "PR_kwDOABC123");
        assert!(pr.merged_at.is_none());
    }

    #[test]
    fn test_parse_pr_draft() {
        let json = r#"{
            "number": 7,
            "html_url": "https://github.com/o/r/pull/7",
            "title": "WIP",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "feat", "label": ""},
            "draft": true,
            "node_id": "PR_kwDOXYZ"
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert!(pr.draft);
        assert!(pr.body.is_none());
    }

    #[test]
    fn test_parse_pr_merged() {
        let json = r#"{
            "number": 99,
            "html_url": "https://github.com/o/r/pull/99",
            "title": "Done",
            "body": "merged",
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "feat", "label": ""},
            "draft": false,
            "node_id": "",
            "merged_at": "2024-06-15T10:30:00Z"
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.merged_at.as_deref(), Some("2024-06-15T10:30:00Z"));
    }
}
