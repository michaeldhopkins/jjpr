#![warn(
    clippy::unwrap_used,
    clippy::redundant_clone,
    clippy::too_many_lines,
    clippy::excessive_nesting,
)]

use std::env;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use std::collections::HashMap;

use jjpr::github::remote;
use jjpr::github::types::PullRequest;
use jjpr::github::{GhCli, GitHub};
use jjpr::graph::change_graph;
use jjpr::jj::{Jj, JjRunner};
use jjpr::submit::{analyze, execute, plan, resolve};

#[derive(Parser)]
#[command(name = "jjpr")]
#[command(about = "Manage stacked pull requests in Jujutsu repositories\n\nRun with no arguments to see your stacks and their PR status on GitHub (read-only).\nUse `jjpr submit` to push, create PRs, and sync stack state.")]
#[command(version, long_about = None, disable_version_flag = true)]
struct Cli {
    /// Print version
    #[arg(short = 'v', short_alias = 'V', long = "version", action = clap::ArgAction::Version)]
    version: (),

    #[command(subcommand)]
    command: Option<Commands>,

    /// Preview changes without executing
    #[arg(long, global = true)]
    dry_run: bool,

    /// Skip fetching remotes before operating
    #[arg(long, global = true)]
    no_fetch: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Push bookmarks and create/update pull requests for a stack.
    /// Idempotent — run repeatedly after rebasing, editing commits, or restacking
    /// to keep PRs in sync.
    Submit {
        /// Bookmark to submit (inferred from working copy if omitted)
        bookmark: Option<String>,

        /// Request reviewers on all PRs in the stack (comma-separated)
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
                no_fetch: cli.no_fetch,
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
        None => cmd_stack_overview(cli.no_fetch),
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
    no_fetch: bool,
    draft_mode: DraftMode,
}

fn cmd_submit(opts: SubmitOptions<'_>) -> Result<()> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path)?;
    let github = GhCli::new();

    if !opts.no_fetch {
        eprintln!("Fetching remotes...");
        jj.git_fetch()?;
    }

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

    let interactive = std::io::stdout().is_terminal();
    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, interactive)?;

    let submission_plan = plan::create_submission_plan(
        &github,
        &segments,
        &remote_name,
        &repo_info,
        &default_branch,
        matches!(opts.draft_mode, DraftMode::Draft),
        matches!(opts.draft_mode, DraftMode::Ready),
        opts.reviewers,
    )?;

    if opts.bookmark.is_some() {
        println!("Submitting stack for '{target_bookmark}'...\n");
    }
    execute::execute_submission_plan(&jj, &github, &submission_plan, opts.reviewers, opts.dry_run)?;
    println!("\nDone.");

    Ok(())
}

fn cmd_stack_overview(no_fetch: bool) -> Result<()> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path)?;

    if !no_fetch {
        eprintln!("Fetching remotes...");
        jj.git_fetch()?;
    }

    let graph = change_graph::build_change_graph(&jj)?;

    if graph.stacks.is_empty() {
        println!("No stacks found. Create bookmarks with `jj bookmark set <name>`.");
        return Ok(());
    }

    // Try to resolve GitHub remote for PR info
    let github = GhCli::new();
    let pr_info = try_load_pr_info(&jj, &github, &graph);

    let multi = graph.stacks.len() > 1;
    for (i, stack) in graph.stacks.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if multi {
            println!("Stack {}:", i + 1);
        }
        for segment in &stack.segments {
            let bookmark_names: Vec<&str> =
                segment.bookmarks.iter().map(|b| b.name.as_str()).collect();
            let name = bookmark_names.join(", ");
            let sync_status = if segment.bookmarks.iter().all(|b| b.is_synced) {
                "synced"
            } else {
                "needs push"
            };
            let change_count = segment.changes.len();

            let pr_label = segment
                .bookmarks
                .first()
                .and_then(|b| pr_info.as_ref()?.get(&b.name))
                .map(|pr| {
                    let state = if pr.draft { "draft" } else { "open" };
                    format!(", #{} {state}", pr.number)
                })
                .unwrap_or_default();

            println!(
                "  {} ({} change{}{}, {})",
                name,
                change_count,
                if change_count == 1 { "" } else { "s" },
                pr_label,
                sync_status
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

fn try_load_pr_info(
    jj: &dyn Jj,
    github: &dyn GitHub,
    graph: &change_graph::ChangeGraph,
) -> Option<HashMap<String, PullRequest>> {
    let remotes = jj.get_git_remotes().ok()?;
    let (_remote_name, repo_info) = remote::resolve_remote(&remotes, None).ok()?;

    let all_prs = match github.list_open_prs(&repo_info.owner, &repo_info.repo) {
        Ok(prs) => prs,
        Err(_) => {
            if !graph.stacks.is_empty() && github.get_authenticated_user().is_err() {
                eprintln!("hint: run `jjpr auth test` to see PR status in stack overview");
            }
            return Some(HashMap::new());
        }
    };

    let owner_prefix = format!("{}:", repo_info.owner);
    let map: HashMap<String, PullRequest> = all_prs
        .into_iter()
        .filter(|pr| pr.head.label.starts_with(&owner_prefix) || pr.head.label.is_empty())
        .map(|pr| (pr.head.ref_name.clone(), pr))
        .collect();

    Some(map)
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
