use std::process::Command;

use anyhow::{Context, Result};

use super::types::{ChecksStatus, IssueComment, MergeMethod, PrMergeability, PullRequest, ReviewSummary};
use super::Forge;

/// GitHub implementation that shells out to the `gh` CLI.
#[derive(Default)]
pub struct GhCli;

impl GhCli {
    pub fn new() -> Self {
        Self
    }

    fn run_gh(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("gh")
            .args(args)
            .output()
            .context("failed to run gh. Install it: https://cli.github.com")?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh {} failed: {}", args.join(" "), stderr.trim())
        }
    }
}

impl Forge for GhCli {
    fn list_open_prs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<PullRequest>> {
        let endpoint = format!("repos/{owner}/{repo}/pulls?state=open");
        let output = self.run_gh(&["api", &endpoint, "--paginate"])?;
        serde_json::from_str(&output).context("failed to parse PR list response")
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
        let endpoint = format!("repos/{owner}/{repo}/pulls");
        let title_arg = format!("title={title}");
        let head_arg = format!("head={head}");
        let base_arg = format!("base={base}");
        let body_arg = format!("body={body}");
        let mut args = vec![
            "api", &endpoint,
            "-f", &title_arg,
            "-f", &head_arg,
            "-f", &base_arg,
            "-f", &body_arg,
        ];
        if draft {
            args.push("-F");
            args.push("draft=true");
        }
        let output = self.run_gh(&args)?;
        serde_json::from_str(&output).context("failed to parse created PR response")
    }

    fn update_pr_base(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        base: &str,
    ) -> Result<()> {
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}");
        self.run_gh(&[
            "api", &endpoint,
            "-X", "PATCH",
            "-f", &format!("base={base}"),
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
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/requested_reviewers");
        let mut args = vec!["api", &endpoint, "-X", "POST"];
        let formatted: Vec<String> = reviewers
            .iter()
            .map(|r| format!("reviewers[]={r}"))
            .collect();
        for reviewer_arg in &formatted {
            args.push("-f");
            args.push(reviewer_arg);
        }
        self.run_gh(&args)?;
        Ok(())
    }

    fn list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>> {
        let endpoint = format!("repos/{owner}/{repo}/issues/{number}/comments");
        let output = self.run_gh(&["api", &endpoint, "--paginate"])?;
        serde_json::from_str(&output).context("failed to parse comments response")
    }

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let endpoint = format!("repos/{owner}/{repo}/issues/{number}/comments");
        let output = self.run_gh(&[
            "api", &endpoint,
            "-f", &format!("body={body}"),
        ])?;
        serde_json::from_str(&output).context("failed to parse created comment response")
    }

    fn update_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        let endpoint = format!("repos/{owner}/{repo}/issues/comments/{comment_id}");
        self.run_gh(&[
            "api", &endpoint,
            "-X", "PATCH",
            "-f", &format!("body={body}"),
        ])?;
        Ok(())
    }

    fn update_pr_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}");
        self.run_gh(&[
            "api", &endpoint,
            "-X", "PATCH",
            "-f", &format!("body={body}"),
        ])?;
        Ok(())
    }

    fn mark_pr_ready(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<()> {
        // Fetch node_id from the PR (GitHub GraphQL requires it)
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}");
        let output = self.run_gh(&["api", &endpoint])?;
        let pr: serde_json::Value = serde_json::from_str(&output)
            .context("failed to parse PR response for node_id")?;
        let node_id = pr["node_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("PR response missing node_id field"))?;

        let query = "mutation($id: ID!) { markPullRequestReadyForReview(input: { pullRequestId: $id }) { clientMutationId } }";
        let id_arg = format!("id={node_id}");
        self.run_gh(&["api", "graphql", "-f", &format!("query={query}"), "-F", &id_arg])?;
        Ok(())
    }

    fn find_merged_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let endpoint = format!(
            "repos/{owner}/{repo}/pulls?head={owner}:{head}&state=closed"
        );
        let output = self.run_gh(&["api", &endpoint])?;
        let prs: Vec<PullRequest> = serde_json::from_str(&output)
            .context("failed to parse closed PR list response")?;
        Ok(prs.into_iter().find(|pr| pr.merged_at.is_some()))
    }

    fn get_authenticated_user(&self) -> Result<String> {
        let output = self.run_gh(&["api", "user"])?;
        let user: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse user response")?;
        user["login"]
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
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/merge");
        let method_arg = format!("merge_method={method}");
        self.run_gh(&[
            "api", &endpoint,
            "-X", "PUT",
            "-f", &method_arg,
        ])?;
        Ok(())
    }

    fn get_pr_checks_status(
        &self,
        owner: &str,
        repo: &str,
        head_ref: &str,
    ) -> Result<ChecksStatus> {
        let check_runs_endpoint =
            format!("repos/{owner}/{repo}/commits/{head_ref}/check-runs");
        let check_runs_output = self.run_gh(&["api", &check_runs_endpoint])?;
        let check_runs: serde_json::Value = serde_json::from_str(&check_runs_output)
            .context("failed to parse check-runs response")?;

        let status_endpoint =
            format!("repos/{owner}/{repo}/commits/{head_ref}/status");
        let status_output = self.run_gh(&["api", &status_endpoint])?;
        let status: serde_json::Value = serde_json::from_str(&status_output)
            .context("failed to parse commit status response")?;

        let runs = check_runs["check_runs"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default();
        let statuses = status["statuses"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default();

        if runs.is_empty() && statuses.is_empty() {
            return Ok(ChecksStatus::None);
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
            Ok(ChecksStatus::Fail)
        } else if has_pending {
            Ok(ChecksStatus::Pending)
        } else {
            Ok(ChecksStatus::Pass)
        }
    }

    fn get_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<ReviewSummary> {
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/reviews");
        let output = self.run_gh(&["api", &endpoint, "--paginate"])?;
        let reviews: Vec<serde_json::Value> = serde_json::from_str(&output)
            .context("failed to parse reviews response")?;

        // Track each reviewer's latest meaningful review state.
        // COMMENTED and PENDING don't change approval status on GitHub,
        // so we skip them to avoid overwriting a valid APPROVED state.
        let mut latest: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for review in &reviews {
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
        let endpoint = format!("repos/{owner}/{repo}/pulls/{number}");
        let output = self.run_gh(&["api", &endpoint])?;
        let pr: serde_json::Value = serde_json::from_str(&output)
            .context("failed to parse PR mergeability response")?;

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
