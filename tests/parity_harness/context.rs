use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use super::scenario::{AdminMergeMethod, StackEntry};

pub const OWNER: &str = "michaeldhopkins";
pub const REPO: &str = "jjpr-testing-environment";

/// Per-scenario test context. Clones the testing repo into a temp dir,
/// mints a unique bookmark prefix so concurrent runs don't collide, and
/// cleans up bookmarks/PRs/branches on Drop.
pub struct ParityContext {
    pub prefix: String,
    pub repo_path: PathBuf,
    _parent: TempDir,
}

impl ParityContext {
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};

        // 24 bits of timestamp + 24 bits of pid keeps prefixes short while
        // remaining collision-free for parallel test runs on CI.
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_secs();
        let pid = std::process::id() as u64;
        let prefix = format!("p{:06x}{:06x}", ts & 0xFFFFFF, pid & 0xFFFFFF);

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

        Self {
            prefix,
            repo_path,
            _parent: parent,
        }
    }

    /// Apply the prefix to the scenario-supplied bookmark name so concurrent
    /// runs and the cleanup-on-Drop logic both see a unique label.
    pub fn prefixed(&self, name: &str) -> String {
        format!("{}-{}", self.prefix, name)
    }

    /// Build the stack described by the scenario. Each entry creates one
    /// commit on a fresh change and sets the (prefixed) bookmark on it.
    pub fn build_stack(&self, entries: &[StackEntry]) {
        for entry in entries {
            let prefixed_file = format!("{}-{}", self.prefix, entry.file);
            std::fs::write(self.repo_path.join(&prefixed_file), &entry.content)
                .expect("write stack file");
            self.run_jj(&["commit", "-m", &entry.message]);
            let bookmark = self.prefixed(&entry.bookmark);
            self.run_jj(&["bookmark", "set", &bookmark, "-r", "@-"]);
        }
    }

    pub fn run_jj(&self, args: &[&str]) -> String {
        let output = Command::new("jj")
            .args(args)
            .current_dir(&self.repo_path)
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

    /// Externally admin-merge the PR whose head is the given prefixed bookmark.
    /// Used in scenarios that simulate "a maintainer merged the bottom of
    /// your stack while you weren't looking."
    pub fn external_admin_merge(&self, bookmark: &str, method: AdminMergeMethod) {
        let full_repo = format!("{OWNER}/{REPO}");
        let pr = find_pr_by_head(bookmark)
            .unwrap_or_else(|| panic!("no open PR for bookmark '{bookmark}'"));
        let number = pr["number"]
            .as_u64()
            .expect("PR number")
            .to_string();
        let status = Command::new("gh")
            .args([
                "pr", "merge", &number,
                "--repo", &full_repo,
                method.gh_flag(),
                "--admin",
            ])
            .status()
            .expect("gh pr merge");
        assert!(status.success(), "gh pr merge --admin failed for {bookmark}");
    }
}

impl Drop for ParityContext {
    fn drop(&mut self) {
        let full_repo = format!("{OWNER}/{REPO}");

        // Close any still-open PRs whose head matches our prefix.
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
                if head.starts_with(&self.prefix)
                    && let Some(n) = pr["number"].as_u64()
                {
                    let _ = Command::new("gh")
                        .args(["pr", "close", &n.to_string(), "--repo", &full_repo])
                        .output();
                }
            }
        }

        // Delete remote branches whose name starts with our prefix.
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

/// Look up the open PR whose head ref equals `head`. Returns the raw
/// JSON object so callers can pull whichever fields they need.
pub fn find_pr_by_head(head: &str) -> Option<serde_json::Value> {
    let full_repo = format!("{OWNER}/{REPO}");
    let output = Command::new("gh")
        .args([
            "pr", "list", "--repo", &full_repo, "--head", head,
            "--json", "number,title,baseRefName,headRefName,state",
            "--state", "all",
        ])
        .output()
        .ok()?;
    let prs: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).ok()?;
    prs.into_iter().next()
}

/// Look up a PR with extra detail fields (additions, deletions, commits, mergedAt).
/// Useful for diff-size and merge-state assertions.
pub fn fetch_pr_detail(number: u64) -> Option<serde_json::Value> {
    let full_repo = format!("{OWNER}/{REPO}");
    let output = Command::new("gh")
        .args([
            "pr", "view", &number.to_string(),
            "--repo", &full_repo,
            "--json", "number,state,baseRefName,headRefName,additions,deletions,mergedAt,commits",
        ])
        .output()
        .ok()?;
    serde_json::from_slice(&output.stdout).ok()
}

pub fn list_comments(number: u64) -> Vec<serde_json::Value> {
    let full_repo = format!("{OWNER}/{REPO}");
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{full_repo}/issues/{number}/comments"),
        ])
        .output()
        .expect("gh api list comments");
    serde_json::from_slice(&output.stdout).unwrap_or_default()
}

pub fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn gh_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Path to the parity_scenarios directory, resolved at compile time.
pub fn scenarios_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/parity_scenarios"))
}
