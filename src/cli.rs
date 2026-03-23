use clap::{Parser, Subcommand};

use crate::config::ReconcileStrategy;
use crate::forge::types::MergeMethod;

#[derive(Parser)]
#[command(name = "jjpr")]
#[command(about = "Manage stacked pull requests in Jujutsu repositories")]
#[command(version, disable_version_flag = true)]
#[command(long_about = "\
Manage stacked pull requests in Jujutsu repositories.

Each jj bookmark becomes one pull request. A \"stack\" is a chain of bookmarks \
that jjpr discovers by walking parent commits from your bookmarks toward trunk. \
Commits without bookmarks are folded into the nearest bookmarked ancestor's PR.

Run with no arguments to see your stacks and their PR/MR status (read-only):

    $ jjpr
      auth (1 change, #42 open, needs push)
      profile (2 changes, #41 draft, synced)

Use `jjpr submit` to push bookmarks, create/update PRs, and add stack \
navigation comments. Use `jjpr merge` to land them from the bottom up.")]
pub struct Cli {
    /// Print version
    #[arg(short = 'v', short_alias = 'V', long = "version", action = clap::ArgAction::Version)]
    pub version: (),

    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Preview changes without executing
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Skip fetching remotes before operating
    #[arg(long, global = true)]
    pub no_fetch: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Push bookmarks and create/update pull requests for a stack
    #[command(long_about = "\
Push bookmarks and create/update pull requests for a stack.

Each bookmark in the stack gets its own PR. Commits between two bookmarks \
are grouped into the upper bookmark's PR. If you have 6 commits but only \
one bookmark, you get one PR containing all 6 commits.

When no bookmark is specified, jjpr infers the target from your working \
copy — it finds which stack overlaps with `trunk()..@` and submits up to \
the topmost bookmark. Your working copy must be at or below a bookmarked \
commit (an empty commit above the stack won't match any bookmark).

Each PR receives a stack navigation comment showing its position:

    This PR is part of a stack:
    1. `profile` <-- this PR
    2. `auth`

Submit is idempotent — run it after rebasing, editing commits, or \
restacking to push updates, fix PR base branches, and sync descriptions.

Foreign base detection: if your stack builds on a coworker's remote \
branch, jjpr targets your bottom PR at their branch instead of main. \
Use --base to override this when the coworker hasn't pushed yet.

Examples:
    jjpr submit              # submit the stack under your working copy
    jjpr submit auth         # submit the stack ending at bookmark 'auth'
    jjpr submit --draft      # create new PRs as drafts
    jjpr submit --reviewer alice,bob  # request reviewers on all PRs
    jjpr submit --dry-run    # preview what would happen

Reviewer requests are idempotent — re-running with --reviewer only \
affects PRs where the reviewer isn't already requested.")]
    Submit {
        /// Bookmark to submit (inferred from working copy if omitted)
        bookmark: Option<String>,

        /// Request reviewers on all PRs in the stack (comma-separated)
        #[arg(long, value_delimiter = ',')]
        reviewer: Vec<String>,

        /// Git remote name
        #[arg(long)]
        remote: Option<String>,

        /// Create new PRs as drafts
        #[arg(long)]
        draft: bool,

        /// Mark existing draft PRs as ready for review
        #[arg(long, conflicts_with = "draft")]
        ready: bool,

        /// Base branch for the bottom of the stack
        ///
        /// Auto-detected from remote bookmarks if omitted. When your stack
        /// builds on a coworker's branch, jjpr targets that branch automatically.
        /// Use this flag to override (e.g., when the branch isn't pushed yet).
        #[arg(long)]
        base: Option<String>,
    },
    /// Show stack status with CI, review, and mergeability details
    #[command(long_about = "\
Show your stacks with detailed CI, review, and mergeability status.

This is the same output as running `jjpr` with no arguments. The `status` \
subcommand exists for discoverability — both forms are identical.

Example output:
    $ jjpr status
      auth (1 change, #42 open, synced)
        ✓ mergeable  ✓ CI passing  ✓ 1 approval
      profile (2 changes, #43 open, needs push)
        ✗ CI pending  ✗ 0/1 approvals")]
    Status {},
    /// Merge a stack of PRs from the bottom up
    #[command(long_about = "\
Merge a stack of PRs from the bottom up.

Merges the bottommost mergeable PR, fetches the updated default branch, \
syncs the remaining stack (merge commits by default, or rebase with \
--reconcile-strategy rebase), pushes, retargets the next PR's base, \
and repeats until blocked or done.

Before merging each PR, jjpr checks:
  - PR is not a draft
  - CI checks pass (skip with --no-ci-check)
  - Required approvals met (override with --required-approvals)
  - No changes requested
  - No merge conflicts

Idempotent — re-run after CI passes or reviews are approved to continue.

Examples:
    jjpr merge                        # merge from the bottom up
    jjpr merge --merge-method rebase  # use rebase instead of squash
    jjpr merge --no-ci-check          # merge even if CI is pending
    jjpr merge --dry-run              # preview what would happen")]
    Merge {
        /// Bookmark to merge (inferred from working copy if omitted)
        bookmark: Option<String>,

        /// Merge method (overrides config file)
        #[arg(long, value_enum)]
        merge_method: Option<MergeMethod>,

        /// Required approvals before merging (overrides config file)
        #[arg(long)]
        required_approvals: Option<u32>,

        /// Skip CI check requirement
        #[arg(long)]
        no_ci_check: bool,

        /// Git remote name
        #[arg(long)]
        remote: Option<String>,

        /// Base branch for the bottom of the stack
        ///
        /// Auto-detected from remote bookmarks if omitted. When your stack
        /// builds on a coworker's branch, jjpr targets that branch automatically.
        /// Use this flag to override (e.g., when the branch isn't pushed yet).
        #[arg(long)]
        base: Option<String>,

        /// How to sync the remaining stack after each merge (overrides config file)
        ///
        /// "merge" (default): creates merge commits on downstream branches — no force pushes.
        /// "rebase": rebases downstream commits — causes force pushes on GitHub.
        #[arg(long, value_enum)]
        reconcile_strategy: Option<ReconcileStrategy>,

        /// Deprecated: use `jjpr watch` instead
        #[arg(long, hide = true)]
        watch: bool,

        /// Timeout in minutes for --watch mode
        #[arg(long, value_name = "MINUTES", requires = "watch", hide = true)]
        timeout: Option<u64>,

        /// Mark draft PRs as ready before merging
        #[arg(long)]
        ready: bool,
    },
    /// Watch your stack and auto-manage PRs from draft to merge
    #[command(long_about = "\
Watch your stack and auto-manage PRs from draft through merge.

jjpr watch is an always-on assistant for your stack. It runs in a loop, \
polling every 30 seconds, and handles the full PR lifecycle:

  1. Creates draft PRs for bookmarks that don't have PRs yet
  2. Marks draft PRs as ready when CI passes
  3. Merges ready PRs (bottom-up) when approved and mergeable
  4. Syncs the remaining stack after each merge

If a PR needs review approval but has no reviewers, watch will tell you.

Stack comments (showing position in the stack) are added automatically when \
the stack has 2 or more PRs. A single-PR stack looks like a normal PR.

Press Ctrl+C to exit. Use --timeout to set a maximum watch duration.

Examples:
    jjpr watch                          # watch the stack under your working copy
    jjpr watch auth                     # watch the stack ending at bookmark 'auth'
    jjpr watch --timeout 60             # stop after 60 minutes
    jjpr watch --no-ci-check            # merge without waiting for CI")]
    Watch {
        /// Bookmark to watch (inferred from working copy if omitted)
        bookmark: Option<String>,

        /// Git remote name
        #[arg(long)]
        remote: Option<String>,

        /// Base branch for the bottom of the stack
        #[arg(long)]
        base: Option<String>,

        /// Merge method (overrides config file)
        #[arg(long, value_enum)]
        merge_method: Option<MergeMethod>,

        /// Required approvals before merging (overrides config file)
        #[arg(long)]
        required_approvals: Option<u32>,

        /// Skip CI check requirement
        #[arg(long)]
        no_ci_check: bool,

        /// How to sync the remaining stack after each merge (overrides config file)
        #[arg(long, value_enum)]
        reconcile_strategy: Option<ReconcileStrategy>,

        /// Timeout in minutes (default: no timeout)
        #[arg(long, value_name = "MINUTES")]
        timeout: Option<u64>,
    },
    /// Manage forge authentication
    #[command(long_about = "\
Manage forge authentication.

jjpr authenticates via token environment variables or CLI credential stores:

  GitHub:          GITHUB_TOKEN or GH_TOKEN (fallback: `gh auth login`)
  GitLab:          GITLAB_TOKEN            (fallback: `glab auth login`)
  Forgejo/Codeberg: FORGEJO_TOKEN

Use `jjpr auth test` to verify credentials, `jjpr auth setup` for full \
setup instructions.")]
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Manage jjpr configuration
    #[command(long_about = "\
Manage jjpr configuration.

jjpr uses an optional TOML config file for merge settings. Global config \
lives at ~/.config/jjpr/config.toml (or $XDG_CONFIG_HOME/jjpr/config.toml).

A repo-local config at .jj/jjpr.toml overrides global settings — useful \
for setting forge type and token env var for self-hosted instances.

Use `jjpr config init` to create the global config with defaults, or \
`jjpr config init --repo` for repo-local config. CLI flags always override \
config file values.")]
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// Test forge authentication and show the authenticated user
    Test,
    /// Show authentication setup instructions for the detected forge
    Setup,
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Create a default config file at ~/.config/jjpr/config.toml
    Init {
        /// Create repo-local config at .jj/jjpr.toml instead of global config
        #[arg(long)]
        repo: bool,
    },
}
