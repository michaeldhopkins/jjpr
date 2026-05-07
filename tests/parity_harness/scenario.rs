use serde::Deserialize;

/// One scenario file under `tests/parity_scenarios/`. Loaded as TOML.
#[derive(Debug, Deserialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,

    /// Stack to build before running setup or the command-under-test.
    /// Ordered base → top.
    pub stack: Vec<StackEntry>,

    /// Optional setup steps run after the stack is built and before the
    /// command-under-test. Use these to put the forge/local repo into a
    /// non-default starting state (initial submit, external merges, etc.).
    #[serde(default)]
    pub setup: Vec<SetupStep>,

    /// The jjpr subcommand under test.
    pub run: RunSpec,

    /// Assertions to verify after the run.
    pub expect: Expectations,
}

#[derive(Debug, Deserialize)]
pub struct StackEntry {
    pub bookmark: String,
    pub file: String,
    pub content: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SetupStep {
    /// Run `jjpr submit [...extra_args]` with the harness's targeting.
    Submit {
        #[serde(default)]
        extra_args: Vec<String>,
    },
    /// `gh pr merge --admin <bookmark>` to simulate an external merge.
    /// `method` is one of squash | merge | rebase.
    ExternalAdminMerge {
        bookmark: String,
        method: AdminMergeMethod,
    },
    /// Repoint a git remote to a different URL. Be aware: this also
    /// changes the owner/repo jjpr derives for forge API calls, so for
    /// most "break-the-fetch" scenarios prefer `set_git_config` instead.
    SetRemoteUrl { remote: String, url: String },
    /// Set a key in the repo-local git config, e.g.
    /// `core.sshCommand = "/bin/false"` to deterministically break
    /// SSH-backed git fetch/push without touching the remote URL.
    /// The forge API path is unaffected (it uses GITHUB_TOKEN over HTTPS).
    SetGitConfig { key: String, value: String },
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum AdminMergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl AdminMergeMethod {
    pub fn gh_flag(self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RunSpec {
    pub command: JjprCommand,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Only meaningful for `watch`. Forces `--timeout` so the loop exits.
    /// Default: 1 minute.
    #[serde(default)]
    pub timeout_minutes: Option<u64>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum JjprCommand {
    Submit,
    Merge,
    Watch,
}

impl JjprCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::Merge => "merge",
            Self::Watch => "watch",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Expectations {
    /// "success" | "failure". Default: "success".
    #[serde(default = "default_exit")]
    pub exit_status: ExitStatus,

    /// Substrings expected in stderr (each must be present).
    #[serde(default)]
    pub stderr_contains: Vec<String>,

    /// Substrings forbidden in stderr (none must be present).
    #[serde(default)]
    pub stderr_not_contains: Vec<String>,

    /// Substrings expected in stdout (each must be present).
    #[serde(default)]
    pub stdout_contains: Vec<String>,

    /// Substrings forbidden in stdout (none must be present).
    #[serde(default)]
    pub stdout_not_contains: Vec<String>,

    /// PR-level expectations, keyed by bookmark.
    #[serde(default, rename = "pr")]
    pub prs: Vec<PrExpectation>,

    /// Stack-comment expectations, keyed by bookmark.
    #[serde(default, rename = "comment")]
    pub comments: Vec<CommentExpectation>,
}

fn default_exit() -> ExitStatus {
    ExitStatus::Success
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Success,
    Failure,
}

#[derive(Debug, Deserialize)]
pub struct PrExpectation {
    pub bookmark: String,

    /// "open" | "merged" | "closed" | "absent". If absent, no state check.
    pub state: Option<PrStateExpect>,

    /// Expected base ref name (e.g. "main", "auth").
    pub base: Option<String>,

    /// Maximum allowed commit count on the PR. Guards against bloated diffs
    /// when local rebase fails to bring the PR up to date with its new base.
    pub commit_count_max: Option<u64>,

    /// Maximum allowed (additions + deletions) on the PR. Stronger guard
    /// against bloated diffs.
    pub diff_lines_max: Option<u64>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PrStateExpect {
    Open,
    Merged,
    Closed,
    Absent,
}

#[derive(Debug, Deserialize)]
pub struct CommentExpectation {
    pub bookmark: String,

    /// Substrings expected in the stack-info comment body.
    #[serde(default)]
    pub contains: Vec<String>,

    /// Substrings forbidden in the stack-info comment body.
    #[serde(default)]
    pub not_contains: Vec<String>,
}
