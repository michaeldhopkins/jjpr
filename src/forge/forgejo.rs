use anyhow::{Context, Result};

use super::http::ForgeClient;
use super::types::{
    ChecksStatus, IssueComment, MergeMethod, PrMergeability, PullRequest, ReviewSummary,
};
use super::Forge;

/// Forgejo/Codeberg implementation using direct HTTP via `ForgeClient`.
pub struct ForgejoForge {
    client: ForgeClient,
}

impl ForgejoForge {
    pub fn new(client: ForgeClient) -> Self {
        Self { client }
    }
}

/// Parse the Forgejo combined commit status into a `ChecksStatus`.
fn parse_combined_status(combined: &serde_json::Value) -> ChecksStatus {
    let statuses = combined["statuses"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default();

    if statuses.is_empty() {
        return ChecksStatus::None;
    }

    let mut has_pending = false;
    let mut has_failure = false;

    for s in statuses {
        match s["status"].as_str() {
            Some("success") => {}
            Some("pending") => has_pending = true,
            Some("error") | Some("failure") => has_failure = true,
            Some("warning") => {}
            _ => has_pending = true,
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

/// Parse Forgejo review objects into a `ReviewSummary`.
fn parse_reviews(items: &[serde_json::Value]) -> ReviewSummary {
    let mut latest: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for review in items {
        let user = review["user"]["login"].as_str().unwrap_or_default();
        let state = review["state"].as_str().unwrap_or_default();
        if !user.is_empty()
            && matches!(
                state,
                "APPROVED" | "REQUEST_CHANGES" | "REJECTED"
            )
        {
            latest.insert(user.to_string(), state.to_string());
        }
    }

    let approved_count = latest.values().filter(|s| *s == "APPROVED").count() as u32;
    let changes_requested = latest
        .values()
        .any(|s| s == "REQUEST_CHANGES" || s == "REJECTED");

    ReviewSummary {
        approved_count,
        changes_requested,
    }
}

/// Parse a Forgejo PR JSON object into a `PrMergeability`.
fn parse_mergeability(pr: &serde_json::Value) -> PrMergeability {
    let mergeable = pr["mergeable"].as_bool();

    PrMergeability {
        mergeable,
        mergeable_state: if mergeable == Some(true) {
            "clean".to_string()
        } else if mergeable == Some(false) {
            "dirty".to_string()
        } else {
            "unknown".to_string()
        },
    }
}

impl Forge for ForgejoForge {
    fn list_open_prs(&self, owner: &str, repo: &str) -> Result<Vec<PullRequest>> {
        let path = format!("repos/{owner}/{repo}/pulls?state=open");
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
        let json_body = serde_json::json!({
            "title": title,
            "body": body,
            "head": head,
            "base": base,
            "draft": draft,
        });
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
        let json_body = serde_json::json!({ "base": base });
        self.client.patch(&path, &json_body)?;
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
        let json_body = serde_json::json!({ "reviewers": reviewers });
        self.client.post(&path, &json_body)?;
        Ok(())
    }

    fn list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>> {
        let path = format!("repos/{owner}/{repo}/issues/{number}/comments");
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
        let json_body = serde_json::json!({ "body": body });
        let output = self.client.post(&path, &json_body)?;
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
        let json_body = serde_json::json!({ "body": body });
        self.client.patch(&path, &json_body)?;
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
        let json_body = serde_json::json!({ "body": body });
        self.client.patch(&path, &json_body)?;
        Ok(())
    }

    fn mark_pr_ready(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        let json_body = serde_json::json!({ "draft": false });
        self.client.patch(&path, &json_body)?;
        Ok(())
    }

    fn get_authenticated_user(&self) -> Result<String> {
        let output = self.client.get("user")?;
        output["login"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("user response missing login field"))
    }

    fn find_merged_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        // Forgejo doesn't support filtering closed PRs by head branch, so we
        // paginate and scan. Cap at 5 pages (250 PRs) to avoid runaway requests
        // on repos with many closed PRs.
        let base_path = format!("repos/{owner}/{repo}/pulls?state=closed");
        for page in 1..=5u32 {
            let paged = format!("{base_path}&page={page}&limit=50");
            let body = self.client.get(&paged)?;
            let prs: Vec<PullRequest> =
                serde_json::from_value(body).context("failed to parse closed PR list response")?;
            if prs.is_empty() {
                break;
            }
            if let Some(pr) = prs
                .into_iter()
                .find(|pr| pr.head.ref_name == head && pr.merged_at.is_some())
            {
                return Ok(Some(pr));
            }
        }
        Ok(None)
    }

    fn merge_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        method: MergeMethod,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}/merge");
        let do_value = match method {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
        };
        let json_body = serde_json::json!({ "Do": do_value });
        self.client.post(&path, &json_body)?;
        Ok(())
    }

    fn get_pr_checks_status(
        &self,
        owner: &str,
        repo: &str,
        head_ref: &str,
    ) -> Result<ChecksStatus> {
        let path = format!("repos/{owner}/{repo}/commits/{head_ref}/status");
        let output = self.client.get(&path)?;
        Ok(parse_combined_status(&output))
    }

    fn get_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<ReviewSummary> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}/reviews");
        let items = self.client.get_paginated(&path)?;
        Ok(parse_reviews(&items))
    }

    fn get_pr_mergeability(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrMergeability> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        let output = self.client.get(&path)?;
        Ok(parse_mergeability(&output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- JSON fixture tests: verify parsing without any HTTP ---

    const FORGEJO_PR_RESPONSE: &str = r#"{
        "number": 42,
        "html_url": "https://codeberg.org/owner/repo/pulls/42",
        "title": "Add authentication",
        "body": "Implements basic auth flow",
        "base": {
            "ref": "main",
            "label": "owner:main"
        },
        "head": {
            "ref": "auth",
            "label": "owner:auth"
        },
        "draft": false,
        "merged_at": null
    }"#;

    const FORGEJO_DRAFT_PR: &str = r#"{
        "number": 7,
        "html_url": "https://codeberg.org/owner/repo/pulls/7",
        "title": "Draft feature",
        "body": null,
        "base": {
            "ref": "develop",
            "label": "owner:develop"
        },
        "head": {
            "ref": "draft-feature",
            "label": "owner:draft-feature"
        },
        "draft": true,
        "merged_at": null
    }"#;

    const FORGEJO_MERGED_PR: &str = r#"{
        "number": 99,
        "html_url": "https://codeberg.org/owner/repo/pulls/99",
        "title": "Already merged",
        "body": "This was merged",
        "base": {
            "ref": "main",
            "label": "owner:main"
        },
        "head": {
            "ref": "old-feature",
            "label": "owner:old-feature"
        },
        "draft": false,
        "merged_at": "2024-06-15T10:30:00Z"
    }"#;

    const FORGEJO_FORK_PR: &str = r#"{
        "number": 15,
        "html_url": "https://codeberg.org/owner/repo/pulls/15",
        "title": "Fork contribution",
        "body": "From a fork",
        "base": {
            "ref": "main",
            "label": "owner:main"
        },
        "head": {
            "ref": "feature",
            "label": "fork-owner:feature"
        },
        "draft": false,
        "merged_at": null
    }"#;

    #[test]
    fn test_parse_pr_basic_fields() {
        let pr: PullRequest = serde_json::from_str(FORGEJO_PR_RESPONSE).unwrap();

        assert_eq!(pr.number, 42);
        assert_eq!(pr.html_url, "https://codeberg.org/owner/repo/pulls/42");
        assert_eq!(pr.title, "Add authentication");
        assert_eq!(pr.body.as_deref(), Some("Implements basic auth flow"));
        assert_eq!(pr.base.ref_name, "main");
        assert_eq!(pr.head.ref_name, "auth");
        assert!(!pr.draft);
        assert!(pr.merged_at.is_none());
        assert!(pr.node_id.is_empty());
    }

    #[test]
    fn test_parse_pr_draft() {
        let pr: PullRequest = serde_json::from_str(FORGEJO_DRAFT_PR).unwrap();

        assert_eq!(pr.number, 7);
        assert!(pr.draft);
        assert!(pr.body.is_none());
        assert_eq!(pr.base.ref_name, "develop");
    }

    #[test]
    fn test_parse_pr_merged() {
        let pr: PullRequest = serde_json::from_str(FORGEJO_MERGED_PR).unwrap();

        assert_eq!(pr.number, 99);
        assert_eq!(pr.merged_at.as_deref(), Some("2024-06-15T10:30:00Z"));
    }

    #[test]
    fn test_parse_pr_fork_label() {
        let pr: PullRequest = serde_json::from_str(FORGEJO_FORK_PR).unwrap();

        assert_eq!(pr.head.label, "fork-owner:feature");
    }

    #[test]
    fn test_parse_pr_same_repo_label() {
        let pr: PullRequest = serde_json::from_str(FORGEJO_PR_RESPONSE).unwrap();

        assert_eq!(pr.head.label, "owner:auth");
    }

    #[test]
    fn test_fork_filtered_by_build_pr_map() {
        let same_repo: PullRequest = serde_json::from_str(FORGEJO_PR_RESPONSE).unwrap();
        let fork: PullRequest = serde_json::from_str(FORGEJO_FORK_PR).unwrap();

        let map = crate::forge::build_pr_map(vec![same_repo, fork], "owner");
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("auth"));
    }

    #[test]
    fn test_parse_pr_list() {
        let json = format!("[{FORGEJO_PR_RESPONSE}, {FORGEJO_DRAFT_PR}]");
        let prs: Vec<PullRequest> = serde_json::from_str(&json).unwrap();

        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[1].number, 7);
    }

    #[test]
    fn test_parse_comment() {
        let json = r#"{"id": 301, "body": "Looks good to me!"}"#;
        let comment: IssueComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.id, 301);
        assert_eq!(comment.body.as_deref(), Some("Looks good to me!"));
    }

    #[test]
    fn test_parse_comment_stack_marker() {
        let json = r#"{"id": 500, "body": "<!-- jjpr:stack-info -->\nStack content"}"#;
        let comment: IssueComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.id, 500);
        assert!(comment
            .body
            .as_deref()
            .unwrap()
            .contains("<!-- jjpr:stack-info -->"));
    }

    #[test]
    fn test_ci_status_mapping() {
        let cases = vec![
            (vec!["success"], ChecksStatus::Pass),
            (vec!["pending"], ChecksStatus::Pending),
            (vec!["failure"], ChecksStatus::Fail),
            (vec!["error"], ChecksStatus::Fail),
            (vec!["warning"], ChecksStatus::Pass),
            (vec!["success", "pending"], ChecksStatus::Pending),
            (vec!["success", "failure"], ChecksStatus::Fail),
        ];

        for (statuses, expected) in cases {
            let items: Vec<serde_json::Value> = statuses
                .iter()
                .map(|s| serde_json::json!({"status": s}))
                .collect();
            let combined = serde_json::json!({"statuses": items});
            let result = parse_combined_status(&combined);
            assert_eq!(result, expected, "statuses {statuses:?} should map correctly");
        }
    }

    #[test]
    fn test_ci_status_empty() {
        let combined = serde_json::json!({"statuses": []});
        assert_eq!(parse_combined_status(&combined), ChecksStatus::None);
    }

    #[test]
    fn test_review_counting() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "bob"}, "state": "REQUEST_CHANGES"}),
            serde_json::json!({"user": {"login": "charlie"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "REQUEST_CHANGES"}),
        ];

        let summary = parse_reviews(&reviews);
        assert_eq!(summary.approved_count, 1); // only charlie
        assert!(summary.changes_requested); // alice and bob
    }

    #[test]
    fn test_review_skips_comment_state() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "COMMENTED"}),
        ];

        let summary = parse_reviews(&reviews);
        assert_eq!(summary.approved_count, 1);
        assert!(!summary.changes_requested);
    }

    #[test]
    fn test_mergeability_mapping() {
        let mergeable = serde_json::json!({"mergeable": true});
        let result = parse_mergeability(&mergeable);
        assert_eq!(result.mergeable, Some(true));
        assert_eq!(result.mergeable_state, "clean");

        let not_mergeable = serde_json::json!({"mergeable": false});
        let result = parse_mergeability(&not_mergeable);
        assert_eq!(result.mergeable, Some(false));
        assert_eq!(result.mergeable_state, "dirty");

        let unknown = serde_json::json!({});
        let result = parse_mergeability(&unknown);
        assert_eq!(result.mergeable, None);
        assert_eq!(result.mergeable_state, "unknown");
    }

    #[test]
    fn test_merge_method_do_field() {
        let squash = serde_json::json!({ "Do": "squash" });
        assert_eq!(squash["Do"].as_str().unwrap(), "squash");

        let merge = serde_json::json!({ "Do": "merge" });
        assert_eq!(merge["Do"].as_str().unwrap(), "merge");

        let rebase = serde_json::json!({ "Do": "rebase" });
        assert_eq!(rebase["Do"].as_str().unwrap(), "rebase");
    }
}
