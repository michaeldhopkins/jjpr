use std::process::Command;

use anyhow::{Context, Result};

use super::types::{IssueComment, PullRequest};
use super::GitHub;

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

impl GitHub for GhCli {
    fn find_open_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let endpoint = format!(
            "repos/{owner}/{repo}/pulls?head={owner}:{head}&state=open"
        );
        let output = self.run_gh(&["api", &endpoint])?;
        let prs: Vec<PullRequest> = serde_json::from_str(&output)
            .context("failed to parse PR list response")?;
        Ok(prs.into_iter().next())
    }

    fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<PullRequest> {
        let endpoint = format!("repos/{owner}/{repo}/pulls");
        let output = self.run_gh(&[
            "api", &endpoint,
            "-f", &format!("title={title}"),
            "-f", &format!("head={head}"),
            "-f", &format!("base={base}"),
            "-f", &format!("body={body}"),
        ])?;
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
        issue: u64,
    ) -> Result<Vec<IssueComment>> {
        let endpoint = format!("repos/{owner}/{repo}/issues/{issue}/comments");
        let output = self.run_gh(&["api", &endpoint, "--paginate"])?;
        serde_json::from_str(&output).context("failed to parse comments response")
    }

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        issue: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let endpoint = format!("repos/{owner}/{repo}/issues/{issue}/comments");
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

    fn get_authenticated_user(&self) -> Result<String> {
        let output = self.run_gh(&["api", "user"])?;
        let user: serde_json::Value =
            serde_json::from_str(&output).context("failed to parse user response")?;
        user["login"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("user response missing login field"))
    }
}
