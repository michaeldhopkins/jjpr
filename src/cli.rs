use clap::{Parser, Subcommand};

use crate::forge::types::MergeMethod;

#[derive(Parser)]
#[command(name = "jjpr")]
#[command(about = "Manage stacked pull requests in Jujutsu repositories\n\nRun with no arguments to see your stacks and their PR/MR status (read-only).\nUse `jjpr submit` to push, create PRs/MRs, and sync stack state.")]
#[command(version, long_about = None, disable_version_flag = true)]
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
    /// Push bookmarks and create/update pull requests for a stack.
    /// Idempotent — run repeatedly after rebasing, editing commits, or restacking
    /// to keep PRs in sync.
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

        /// Base branch for the bottom of the stack (auto-detected from remote bookmarks if omitted)
        #[arg(long)]
        base: Option<String>,
    },
    /// Merge a stack of PRs from the bottom up.
    /// Merges the bottommost mergeable PR, fetches, rebases the remaining stack
    /// onto the updated default branch, pushes, and repeats until blocked.
    /// Idempotent — re-run after CI passes or reviews are approved to continue.
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

        /// Base branch for the bottom of the stack (auto-detected from remote bookmarks if omitted)
        #[arg(long)]
        base: Option<String>,
    },
    /// Manage forge authentication
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Manage jjpr configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// Test forge authentication
    Test,
    /// Show authentication setup instructions
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
