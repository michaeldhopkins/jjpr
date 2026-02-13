#![warn(
    clippy::unwrap_used,
    clippy::redundant_clone,
    clippy::too_many_lines,
    clippy::excessive_nesting,
)]

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use jjpr::github::remote;
use jjpr::github::GhCli;
use jjpr::graph::change_graph;
use jjpr::jj::{Jj, JjRunner};
use jjpr::submit::{analyze, execute, plan, resolve};

#[derive(Parser)]
#[command(name = "jjpr")]
#[command(about = "Manage stacked pull requests in Jujutsu repositories")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Preview changes without executing
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Submit a bookmark stack as pull requests
    Submit {
        /// Bookmark to submit (inferred from working copy if omitted)
        bookmark: Option<String>,

        /// Request reviewers (comma-separated usernames)
        #[arg(long, value_delimiter = ',')]
        reviewer: Vec<String>,

        /// Git remote name (must be a GitHub remote)
        #[arg(long)]
        remote: Option<String>,

        /// Create new PRs as drafts
        #[arg(long)]
        draft: bool,

        /// Mark existing draft PRs as ready for review
        #[arg(long, conflicts_with = "draft")]
        ready: bool,
    },
    /// Manage GitHub authentication
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Test GitHub authentication
    Test,
    /// Show authentication setup instructions
    Setup,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Submit {
            bookmark,
            reviewer,
            remote,
            draft,
            ready,
        }) => {
            let draft_mode = match (draft, ready) {
                (true, _) => DraftMode::Draft,
                (_, true) => DraftMode::Ready,
                _ => DraftMode::Normal,
            };
            cmd_submit(SubmitOptions {
                bookmark: bookmark.as_deref(),
                reviewers: &reviewer,
                preferred_remote: remote.as_deref(),
                dry_run: cli.dry_run,
                draft_mode,
            })
        }
        Some(Commands::Auth { command }) => match command {
            AuthCommands::Test => {
                let github = GhCli::new();
                jjpr::auth::test_auth(&github)
            }
            AuthCommands::Setup => {
                jjpr::auth::print_auth_help();
                Ok(())
            }
        },
        None => cmd_stack_overview(),
    }
}

enum DraftMode {
    Normal,
    Draft,
    Ready,
}

struct SubmitOptions<'a> {
    bookmark: Option<&'a str>,
    reviewers: &'a [String],
    preferred_remote: Option<&'a str>,
    dry_run: bool,
    draft_mode: DraftMode,
}

fn cmd_submit(opts: SubmitOptions<'_>) -> Result<()> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path)?;
    let github = GhCli::new();

    let remotes = jj.get_git_remotes()?;
    let (remote_name, repo_info) = remote::resolve_remote(&remotes, opts.preferred_remote)?;

    let default_branch = jj.get_default_branch()?;

    let graph = change_graph::build_change_graph(&jj)?;

    let target_bookmark = match opts.bookmark {
        Some(name) => name.to_string(),
        None => {
            let inferred = analyze::infer_target_bookmark(&graph, &jj)?;
            println!("Submitting stack for '{inferred}' (inferred from working copy)\n");
            inferred
        }
    };

    let analysis = analyze::analyze_submission_graph(&graph, &target_bookmark)?;

    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, false)?;

    let submission_plan = plan::create_submission_plan(
        &github,
        &segments,
        &remote_name,
        &repo_info,
        &default_branch,
        matches!(opts.draft_mode, DraftMode::Draft),
        matches!(opts.draft_mode, DraftMode::Ready),
    )?;

    if opts.bookmark.is_some() {
        println!("Submitting stack for '{target_bookmark}'...\n");
    }
    execute::execute_submission_plan(&jj, &github, &submission_plan, opts.reviewers, opts.dry_run)?;
    println!("\nDone.");

    Ok(())
}

fn cmd_stack_overview() -> Result<()> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path)?;

    let graph = change_graph::build_change_graph(&jj)?;

    if graph.stacks.is_empty() {
        println!("No stacks found. Create bookmarks with `jj bookmark set <name>`.");
        return Ok(());
    }

    for (i, stack) in graph.stacks.iter().enumerate() {
        if i > 0 {
            println!();
        }
        for segment in &stack.segments {
            let bookmark_names: Vec<&str> =
                segment.bookmarks.iter().map(|b| b.name.as_str()).collect();
            let name = bookmark_names.join(", ");
            let status = if segment.bookmarks.iter().all(|b| b.is_synced) {
                "synced"
            } else {
                "needs push"
            };
            let change_count = segment.changes.len();
            println!(
                "  {} ({} change{}, {})",
                name,
                change_count,
                if change_count == 1 { "" } else { "s" },
                status
            );
        }
    }

    if graph.excluded_bookmark_count > 0 {
        println!(
            "\n({} bookmark{} excluded — merge commits in ancestry)",
            graph.excluded_bookmark_count,
            if graph.excluded_bookmark_count == 1 {
                ""
            } else {
                "s"
            }
        );
    }

    Ok(())
}

fn find_repo_root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to get current directory")?;

    let mut path = cwd.as_path();
    loop {
        if path.join(".jj").is_dir() {
            return Ok(path.to_path_buf());
        }
        match path.parent() {
            Some(parent) => path = parent,
            None => anyhow::bail!(
                "not a jj repository (or any parent up to /). \
                 Run `jj git init` to create one."
            ),
        }
    }
}
