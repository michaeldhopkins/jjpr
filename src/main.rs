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

use jjpr::config;
use jjpr::forge::remote;
use jjpr::forge::types::{MergeMethod, PullRequest};
use jjpr::forge::{Forge, ForgeKind, GhCli};
use jjpr::graph::change_graph;
use jjpr::jj::{Jj, JjRunner};
use jjpr::merge;
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

        /// Git remote name (must be a GitHub remote)
        #[arg(long)]
        remote: Option<String>,

        /// Base branch for the bottom of the stack (auto-detected from remote bookmarks if omitted)
        #[arg(long)]
        base: Option<String>,
    },
    /// Manage GitHub authentication
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
enum AuthCommands {
    /// Test GitHub authentication
    Test,
    /// Show authentication setup instructions
    Setup,
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Create a default config file at ~/.config/jjpr/config.toml
    Init,
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
            base,
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
                base_override: base.as_deref(),
            })
        }
        Some(Commands::Merge {
            bookmark,
            merge_method,
            required_approvals,
            no_ci_check,
            remote,
            base,
        }) => {
            let ci_override = if no_ci_check { Some(false) } else { None };
            cmd_merge(
                MergeArgs {
                    bookmark: bookmark.as_deref(),
                    merge_method,
                    required_approvals,
                    ci_pass_override: ci_override,
                    preferred_remote: remote.as_deref(),
                    base_override: base.as_deref(),
                },
                cli.dry_run,
                cli.no_fetch,
            )
        }
        Some(Commands::Auth { command }) => match command {
            AuthCommands::Test => {
                let github = GhCli::new();
                jjpr::auth::test_auth(&github)
            }
            AuthCommands::Setup => {
                jjpr::auth::print_auth_help(ForgeKind::GitHub);
                Ok(())
            }
        },
        Some(Commands::Config { command }) => match command {
            ConfigCommands::Init => cmd_config_init(),
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
    base_override: Option<&'a str>,
}

fn cmd_submit(opts: SubmitOptions<'_>) -> Result<()> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path)?;
    let github = GhCli::new();

    // Infer bookmark before fetching to avoid a slow network round-trip
    // when there's nothing to submit
    let target_bookmark = match opts.bookmark {
        Some(name) => name.to_string(),
        None => {
            let graph = change_graph::build_change_graph(&jj)?;
            match analyze::infer_target_bookmark(&graph, &jj)? {
                Some(inferred) => {
                    println!("Submitting stack for '{inferred}' (inferred from working copy)\n");
                    inferred
                }
                None => {
                    println!("No bookmark found in the working copy's ancestry.");
                    println!("Set a bookmark with `jj bookmark set <name>` or specify one: `jjpr submit <bookmark>`");
                    return Ok(());
                }
            }
        }
    };

    if !opts.no_fetch {
        eprintln!("Fetching remotes...");
        jj.git_fetch()?;
    }

    let remotes = jj.get_git_remotes()?;
    let (remote_name, forge_kind, repo_info) = remote::resolve_remote(&remotes, opts.preferred_remote)?;

    let default_branch = jj.get_default_branch()?;

    let graph = change_graph::build_change_graph(&jj)?;

    let analysis = analyze::analyze_submission_graph(&graph, &target_bookmark)?;

    let interactive = std::io::stdout().is_terminal();
    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, interactive)?;

    let stack_base = opts.base_override.or(analysis.base_branch.as_deref());
    let submission_plan = plan::create_submission_plan(
        &github,
        &segments,
        &remote_name,
        &repo_info,
        forge_kind,
        &default_branch,
        matches!(opts.draft_mode, DraftMode::Draft),
        matches!(opts.draft_mode, DraftMode::Ready),
        opts.reviewers,
        stack_base,
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

    // Try to resolve forge remote for PR info
    let github = GhCli::new();
    let (forge_kind, pr_info) = try_load_pr_info(&jj, &github, &graph)
        .unwrap_or((ForgeKind::GitHub, HashMap::new()));

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
                .and_then(|b| pr_info.get(&b.name))
                .map(|pr| {
                    let state = if pr.draft { "draft" } else { "open" };
                    format!(", {} {state}", forge_kind.format_ref(pr.number))
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
        if let Some(base) = &stack.base_branch {
            println!("  (based on {base})");
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
    github: &dyn Forge,
    graph: &change_graph::ChangeGraph,
) -> Option<(ForgeKind, HashMap<String, PullRequest>)> {
    let remotes = jj.get_git_remotes().ok()?;
    let (_remote_name, forge_kind, repo_info) = remote::resolve_remote(&remotes, None).ok()?;

    let all_prs = match github.list_open_prs(&repo_info.owner, &repo_info.repo) {
        Ok(prs) => prs,
        Err(_) => {
            if !graph.stacks.is_empty() && github.get_authenticated_user().is_err() {
                eprintln!("hint: run `jjpr auth test` to see PR status in stack overview");
            }
            return Some((forge_kind, HashMap::new()));
        }
    };

    Some((forge_kind, jjpr::forge::build_pr_map(all_prs, &repo_info.owner)))
}

struct MergeArgs<'a> {
    bookmark: Option<&'a str>,
    merge_method: Option<MergeMethod>,
    required_approvals: Option<u32>,
    /// `None` = use config, `Some(false)` = `--no-ci-check`
    ci_pass_override: Option<bool>,
    preferred_remote: Option<&'a str>,
    base_override: Option<&'a str>,
}

fn cmd_merge(args: MergeArgs<'_>, dry_run: bool, no_fetch: bool) -> Result<()> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path)?;
    let github = GhCli::new();
    let cfg = config::load_config()?;

    // Infer bookmark before fetching to avoid a slow network round-trip
    // when there's nothing to merge
    let target_bookmark = match args.bookmark {
        Some(name) => name.to_string(),
        None => {
            let graph = change_graph::build_change_graph(&jj)?;
            match analyze::infer_target_bookmark(&graph, &jj)? {
                Some(inferred) => {
                    println!("Merging stack for '{inferred}' (inferred from working copy)\n");
                    inferred
                }
                None => {
                    println!("No bookmark found in the working copy's ancestry.");
                    println!("Set a bookmark with `jj bookmark set <name>` or specify one: `jjpr merge <bookmark>`");
                    return Ok(());
                }
            }
        }
    };

    if !no_fetch {
        eprintln!("Fetching remotes...");
        jj.git_fetch()?;
    }

    let remotes = jj.get_git_remotes()?;
    let (remote_name, forge_kind, repo_info) = remote::resolve_remote(&remotes, args.preferred_remote)?;

    let default_branch = jj.get_default_branch()?;

    let graph = change_graph::build_change_graph(&jj)?;

    let analysis = analyze::analyze_submission_graph(&graph, &target_bookmark)?;

    let interactive = std::io::stdout().is_terminal();
    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, interactive)?;

    let merge_options = merge::plan::MergeOptions {
        merge_method: args.merge_method.unwrap_or(cfg.merge_method),
        required_approvals: args.required_approvals.unwrap_or(cfg.required_approvals),
        require_ci_pass: args.ci_pass_override.unwrap_or(cfg.require_ci_pass),
    };

    let stack_base = args.base_override.or(analysis.base_branch.as_deref());
    let merge_plan = merge::plan::create_merge_plan(
        &github,
        &segments,
        &repo_info,
        forge_kind,
        &default_branch,
        &remote_name,
        &merge_options,
        stack_base,
    )?;

    if args.bookmark.is_some() {
        println!("Merging stack for '{target_bookmark}'...\n");
    }

    let result = merge::execute::execute_merge_plan(
        &jj, &github, &merge_plan, &segments, dry_run,
    )?;

    if result.merged.is_empty() && result.skipped_merged.is_empty() && result.blocked_at.is_none() {
        println!("\nNo PRs to merge in this stack.");
    } else if result.blocked_at.is_some() {
        println!("\nRun `jjpr merge` again once the issue is resolved.");
    } else if result.merged.is_empty() && !result.skipped_merged.is_empty() {
        println!("\nAll PRs in this stack are already merged.");
    } else {
        println!("\nDone \u{2014} {} PR{} merged.", result.merged.len(), if result.merged.len() == 1 { "" } else { "s" });
    }

    Ok(())
}

fn cmd_config_init() -> Result<()> {
    let path = config::write_default_config()?;
    println!("Created default config at {}", path.display());
    println!("Edit it to customize merge behavior.");
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
