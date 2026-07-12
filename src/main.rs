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
use clap::Parser;

use std::collections::HashMap;

use jjpr::cli::{AuthCommands, Cli, Commands, ConfigCommands};
use jjpr::config;
use jjpr::forge::remote;
use jjpr::forge::types::{ChecksStatus, MergeMethod, PrMergeability, PullRequest, RepoInfo, ReviewSummary};
use jjpr::forge::{AuthScheme, Forge, ForgeClient, ForgejoForge, ForgeKind, GitHubForge, GitLabForge, PaginationStyle};
use jjpr::forge::token as forge_token;
use jjpr::graph::change_graph;
use jjpr::jj::types::Bookmark;
use jjpr::jj::{Jj, JjRunner};
use jjpr::merge;
use jjpr::submit::{analyze, execute, plan, resolve};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Submit {
            bookmark,
            reviewer,
            reviewer_scope,
            remote,
            draft,
            ready,
            base,
        }) => {
            // CLI `conflicts_with` makes (draft, ready) = (true, true)
            // unreachable, so the three plan::DraftMode states cover
            // every input combination.
            let draft_mode = match (draft, ready) {
                (true, _) => plan::DraftMode::NewAsDraft,
                (_, true) => plan::DraftMode::MarkExistingReady,
                _ => plan::DraftMode::Default,
            };
            cmd_submit(SubmitOptions {
                bookmark: bookmark.as_deref(),
                reviewers: &reviewer,
                reviewer_scope,
                preferred_remote: remote.as_deref(),
                dry_run: cli.dry_run,
                no_fetch: cli.no_fetch,
                draft_mode,
                base_override: base.as_deref(),
            })
        }
        Some(Commands::Status { bookmark, all }) => {
            cmd_stack_overview(bookmark.as_deref(), all, cli.no_fetch)
        }
        Some(Commands::Merge {
            bookmark,
            merge_method,
            required_approvals,
            no_ci_check,
            remote,
            base,
            reconcile_strategy,
            watch,
            timeout,
            ready,
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
                    reconcile_strategy,
                    watch,
                    timeout,
                    ready,
                },
                cli.dry_run,
                cli.no_fetch,
            )
        }
        Some(Commands::Watch {
            bookmark,
            reviewer,
            reviewer_scope,
            ready,
            remote,
            base,
            merge_method,
            required_approvals,
            no_ci_check,
            reconcile_strategy,
            timeout,
        }) => {
            // --dry-run is a top-level flag; watch is an infinite loop
            // with long-running side effects, so dry-running it has no
            // sensible meaning. Reject explicitly rather than silently
            // ignoring.
            if cli.dry_run {
                anyhow::bail!(
                    "--dry-run is not supported with `jjpr watch` (the loop \
                     is always live). Use `jjpr submit --dry-run` for a \
                     one-shot preview."
                );
            }
            let ci_override = if no_ci_check { Some(false) } else { None };
            cmd_watch(WatchArgs {
                bookmark: bookmark.as_deref(),
                preferred_remote: remote.as_deref(),
                base_override: base.as_deref(),
                merge_method,
                required_approvals,
                ci_pass_override: ci_override,
                reconcile_strategy,
                timeout,
                no_fetch: cli.no_fetch,
                reviewers: &reviewer,
                reviewer_scope,
                ready,
            })
        }
        Some(Commands::Auth { command }) => {
            match command {
                AuthCommands::Test => {
                    let Some(detected) = detect_forge_for_cwd() else {
                        anyhow::bail!(
                            "could not detect forge. Run from a jj repo with a supported remote, \
                             or set forge = \"...\" in .jj/jjpr.toml"
                        );
                    };
                    print_forge_detection(&detected);
                    let forge = build_forge(
                        detected.kind,
                        detected.host.as_deref(),
                        detected.token,
                        detected.token_env_var.as_deref(),
                    )?;
                    jjpr::auth::test_auth(forge.as_ref())
                }
                AuthCommands::Setup => {
                    match detect_forge_for_cwd() {
                        Some(detected) => {
                            print_forge_detection(&detected);
                            jjpr::auth::print_auth_help(detected.kind);
                        }
                        None => jjpr::auth::print_auth_help_all(),
                    }
                    Ok(())
                }
            }
        }
        Some(Commands::Config { command }) => match command {
            ConfigCommands::Init { repo } => {
                if repo {
                    cmd_config_init_repo()
                } else {
                    cmd_config_init()
                }
            }
        },
        None => cmd_stack_overview(None, false, cli.no_fetch),
    }
}

/// Shared setup result used by submit, merge, and watch commands.
struct ResolvedStack {
    jj: JjRunner,
    forge: Box<dyn Forge>,
    forge_kind: ForgeKind,
    remote_name: String,
    repo_info: RepoInfo,
    default_branch: String,
    config: config::Config,
    segments: Vec<jjpr::jj::types::NarrowedSegment>,
    target_bookmark: String,
    stack_base: Option<String>,
}

/// Resolve the target stack: find repo, infer bookmark, fetch, resolve forge,
/// build graph, analyze segments, and resolve bookmark selections.
///
/// Returns `None` if no bookmark is found in the working copy's ancestry
/// (user needs to create one first).
fn resolve_stack(
    bookmark: Option<&str>,
    preferred_remote: Option<&str>,
    no_fetch: bool,
    command_verb: &str,
    snapshot: bool,
) -> Result<Option<ResolvedStack>> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path.clone())?;
    // jjpr is otherwise working-copy-agnostic; for user-invoked commands
    // (submit/merge) snapshot once so we act on the user's latest edits. The
    // autonomous watch loop passes false — it operates on committed state.
    if snapshot {
        jj.snapshot()?;
    }
    let cfg = config::load_config_with_repo(Some(&repo_path))?;

    let target_bookmark = match bookmark {
        Some(name) => name.to_string(),
        None => {
            let graph = change_graph::build_change_graph(&jj)?;
            match analyze::infer_target_bookmark(&graph, &jj)? {
                Some(inferred) => {
                    println!("{command_verb} stack for '{inferred}' (inferred from working copy)\n");
                    inferred
                }
                None => {
                    println!("No bookmark found in the working copy's ancestry.");
                    println!("Set a bookmark with `jj bookmark set <name>` or specify one: `jjpr <command> <bookmark>`");
                    return Ok(None);
                }
            }
        }
    };

    if !no_fetch {
        eprintln!("Fetching remotes...");
        jj.git_fetch()?;
    }

    let remotes = jj.get_git_remotes()?;
    let resolved = resolve_forge(&remotes, &cfg, preferred_remote)?;
    let ResolvedForge { forge, kind: forge_kind, remote_name, repo_info } = resolved;

    let default_branch = jj.get_default_branch()?;
    let graph = change_graph::build_change_graph(&jj)?;
    let analysis = analyze::analyze_submission_graph(&graph, &target_bookmark)?;
    let interactive = std::io::stdout().is_terminal();
    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, interactive)?;
    let stack_base = analysis.base_branch;

    Ok(Some(ResolvedStack {
        jj,
        forge,
        forge_kind,
        remote_name,
        repo_info,
        default_branch,
        config: cfg,
        segments,
        target_bookmark,
        stack_base,
    }))
}

struct SubmitOptions<'a> {
    bookmark: Option<&'a str>,
    reviewers: &'a [String],
    reviewer_scope: jjpr::forge::types::ReviewerScope,
    preferred_remote: Option<&'a str>,
    dry_run: bool,
    no_fetch: bool,
    draft_mode: plan::DraftMode,
    base_override: Option<&'a str>,
}

fn cmd_submit(opts: SubmitOptions<'_>) -> Result<()> {
    let Some(stack) = resolve_stack(opts.bookmark, opts.preferred_remote, opts.no_fetch, "Submitting", true)? else {
        return Ok(());
    };

    // Pre-flight: check for conflicted commits before attempting any pushes
    let conflicted: Vec<_> = stack.segments.iter()
        .flat_map(|seg| seg.changes.iter().filter(|c| c.conflict)
            .map(|c| (seg.bookmark.name.as_str(), c.change_id.as_str(), c.description_first_line.as_str())))
        .collect();
    if !conflicted.is_empty() {
        eprintln!("Error: cannot push; some commits have unresolved conflicts:\n");
        for (bookmark, change_id, desc) in &conflicted {
            eprintln!("  {change_id} ({bookmark}): {desc}");
        }
        eprintln!();
        eprintln!("To resolve: jj edit <change_id>, fix the conflicts, then re-run jjpr submit.");
        anyhow::bail!("unresolved conflicts in stack");
    }

    let stack_base_override = opts.base_override.or(stack.stack_base.as_deref());
    let submission_plan = plan::create_submission_plan(
        stack.forge.as_ref(),
        &stack.segments,
        &stack.remote_name,
        &stack.repo_info,
        stack.forge_kind,
        &stack.default_branch,
        &plan::SubmitOptions {
            draft_mode: opts.draft_mode,
            reviewers: opts.reviewers,
            reviewer_scope: opts.reviewer_scope,
            stack_base: stack_base_override,
            stack_nav: stack.config.stack_nav,
            dry_run: opts.dry_run,
        },
    )?;

    if opts.bookmark.is_some() {
        println!("Submitting stack for '{}'...\n", stack.target_bookmark);
    }
    execute::execute_submission_plan(&stack.jj, stack.forge.as_ref(), &submission_plan)?;
    println!("\nDone.");

    Ok(())
}

fn cmd_stack_overview(bookmark: Option<&str>, all: bool, no_fetch: bool) -> Result<()> {
    let repo_path = find_repo_root()?;
    let jj = JjRunner::new(repo_path.clone())?;
    let cfg = config::load_config_with_repo(Some(&repo_path))?;

    if !no_fetch {
        eprintln!("Fetching remotes...");
        jj.git_fetch()?;
    }

    let graph = change_graph::build_change_graph(&jj)?;

    if graph.stacks.is_empty() {
        println!("No stacks found. Create bookmarks with `jj bookmark set <name>`.");
        return Ok(());
    }

    let stacks_to_show = match analyze::select_stacks_to_show(&graph, bookmark, all, &jj)? {
        analyze::StackScope::Show(stacks) => stacks,
        analyze::StackScope::NoTarget => {
            println!("No bookmark in working copy ancestry.");
            println!("Use `jjpr status --all` to see every local stack, or `jj bookmark set <name>` to mark one.");
            return Ok(());
        }
        analyze::StackScope::Unknown(name) => {
            println!("Bookmark '{name}' not found in any stack.");
            println!("Run `jjpr status --all` to see every local stack.");
            return Ok(());
        }
    };

    // Try to resolve forge remote for PR info
    let info = try_load_pr_info(&jj, &cfg, &graph).unwrap_or(PrInfoResult {
        pr_map: HashMap::new(),
        forge: None,
        repo_info: None,
    });

    // Fetch status for each PR that has forge access
    let mut status_map: HashMap<String, SegmentDisplayStatus> = HashMap::new();
    if let (Some(forge), Some(repo_info)) = (&info.forge, &info.repo_info) {
        for stack in &stacks_to_show {
            for segment in &stack.segments {
                if let Some(bookmark) = segment.bookmarks.first()
                    && let Some(pr) = info.pr_map.get(&bookmark.name)
                {
                    status_map.insert(
                        bookmark.name.clone(),
                        fetch_segment_status(forge.as_ref(), repo_info, pr),
                    );
                }
            }
        }
    }

    let multi = stacks_to_show.len() > 1;
    for (i, stack) in stacks_to_show.iter().enumerate() {
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
            let sync_status = sync_status_label(&segment.bookmarks);
            let change_count = segment.changes.len();

            let pr_label = segment
                .bookmarks
                .first()
                .and_then(|b| info.pr_map.get(&b.name))
                .map(|pr| if pr.draft { ", PR draft" } else { ", PR open" })
                .unwrap_or_default();

            let merge_label = if segment.merge_source_names.is_empty() {
                String::new()
            } else {
                format!(", merge of {}", segment.merge_source_names.join(" + "))
            };

            println!(
                "  {} ({} change{}{}{}, {})",
                name,
                change_count,
                if change_count == 1 { "" } else { "s" },
                merge_label,
                pr_label,
                sync_status
            );

            // Under each segment: the PR link (or a hint when unsubmitted),
            // then the CI/review detail for non-draft PRs.
            if let Some(bookmark) = segment.bookmarks.first() {
                if let Some(pr) = info.pr_map.get(&bookmark.name) {
                    println!("    {}", pr.html_url);
                    if !pr.draft
                        && let Some(status) = status_map.get(&bookmark.name)
                    {
                        let line = format_status_line(status);
                        if !line.is_empty() {
                            println!("{line}");
                        }
                    }
                } else if info.forge.is_some() {
                    // Only when we could actually query the forge and it has no
                    // open PR — not when the forge was unreachable (pr_map is
                    // then empty for every segment, which is not "no PR yet").
                    println!("    no PR yet — run `jjpr submit`");
                }
            }
        }
        if let Some(base) = &stack.base_branch {
            println!("  (based on {base})");
        }
    }

    Ok(())
}

struct PrInfoResult {
    pr_map: HashMap<String, PullRequest>,
    forge: Option<Box<dyn Forge>>,
    repo_info: Option<RepoInfo>,
}

fn try_load_pr_info(
    jj: &dyn Jj,
    cfg: &config::Config,
    graph: &change_graph::ChangeGraph,
) -> Option<PrInfoResult> {
    let remotes = jj.get_git_remotes().ok()?;
    let resolved = resolve_forge(&remotes, cfg, None).ok()?;
    let ResolvedForge { forge, repo_info, .. } = resolved;

    let all_prs = match forge.list_open_prs(&repo_info.owner, &repo_info.repo) {
        Ok(prs) => prs,
        Err(_) => {
            if !graph.stacks.is_empty() && forge.get_authenticated_user().is_err() {
                eprintln!("hint: run `jjpr auth test` to check authentication for stack overview");
            }
            return Some(PrInfoResult {
                pr_map: HashMap::new(),
                forge: None,
                repo_info: None,
            });
        }
    };

    let pr_map = jjpr::forge::build_pr_map(all_prs, &repo_info.owner);
    Some(PrInfoResult {
        pr_map,
        forge: Some(forge),
        repo_info: Some(repo_info),
    })
}

struct SegmentDisplayStatus {
    mergeability: Option<PrMergeability>,
    checks: Option<ChecksStatus>,
    reviews: Option<ReviewSummary>,
}

fn fetch_segment_status(
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    pr: &PullRequest,
) -> SegmentDisplayStatus {
    let mergeability = forge
        .get_pr_mergeability(&repo_info.owner, &repo_info.repo, pr.number)
        .ok();
    let checks = forge
        .get_pr_checks_status(&repo_info.owner, &repo_info.repo,
            if pr.head.sha.is_empty() { &pr.head.ref_name } else { &pr.head.sha })
        .ok();
    let reviews = forge
        .get_pr_reviews(&repo_info.owner, &repo_info.repo, pr.number)
        .ok();
    SegmentDisplayStatus { mergeability, checks, reviews }
}

/// Where a segment's local commits stand relative to the pushed PR branch.
///
/// `is_synced` means the remote bookmark points at the same commit as local;
/// `has_remote` means it was pushed at least once. The three states let a
/// never-pushed segment say so instead of falsely claiming it "needs push".
fn sync_status_label(bookmarks: &[Bookmark]) -> &'static str {
    if !bookmarks.is_empty() && bookmarks.iter().all(|b| b.is_synced) {
        "push up to date"
    } else if bookmarks.iter().any(|b| b.has_remote) {
        "push needs updating"
    } else {
        "not pushed yet"
    }
}

fn format_status_line(status: &SegmentDisplayStatus) -> String {
    let mut parts = Vec::new();

    if let Some(m) = &status.mergeability {
        match m.mergeable {
            Some(true) => parts.push("\u{2713} mergeable".to_string()),
            Some(false) => parts.push("\u{2717} conflicts".to_string()),
            None => parts.push("? mergeability computing".to_string()),
        }
    }

    if let Some(checks) = &status.checks {
        match checks {
            ChecksStatus::Pass => parts.push("\u{2713} CI passing".to_string()),
            ChecksStatus::Fail => parts.push("\u{2717} CI failing".to_string()),
            ChecksStatus::Pending => parts.push("\u{2717} CI pending".to_string()),
            ChecksStatus::None => {}
        }
    }

    if let Some(r) = &status.reviews {
        if r.changes_requested {
            parts.push("\u{26a0} changes requested".to_string());
        }
        parts.push(format!(
            "{} {} approval{}",
            if r.approved_count > 0 { "\u{2713}" } else { "\u{2717}" },
            r.approved_count,
            if r.approved_count == 1 { "" } else { "s" },
        ));
    }

    if parts.is_empty() {
        return String::new();
    }
    format!("    {}", parts.join("  "))
}

struct MergeArgs<'a> {
    bookmark: Option<&'a str>,
    merge_method: Option<MergeMethod>,
    required_approvals: Option<u32>,
    /// `None` = use config, `Some(false)` = `--no-ci-check`
    ci_pass_override: Option<bool>,
    preferred_remote: Option<&'a str>,
    base_override: Option<&'a str>,
    reconcile_strategy: Option<config::ReconcileStrategy>,
    watch: bool,
    timeout: Option<u64>,
    ready: bool,
}

fn cmd_merge(args: MergeArgs<'_>, dry_run: bool, no_fetch: bool) -> Result<()> {
    let Some(stack) = resolve_stack(args.bookmark, args.preferred_remote, no_fetch, "Merging", true)? else {
        return Ok(());
    };

    let merge_options = merge::plan::MergeOptions {
        merge_method: args.merge_method.unwrap_or(stack.config.merge_method),
        required_approvals: args.required_approvals.unwrap_or(stack.config.required_approvals),
        require_ci_pass: args.ci_pass_override.unwrap_or(stack.config.require_ci_pass),
        reconcile_strategy: args.reconcile_strategy.unwrap_or(stack.config.reconcile_strategy),
        ready: args.ready,
    };

    let stack_base_str = args.base_override
        .map(|s| s.to_string())
        .or(stack.stack_base.clone());
    let stack_base = stack_base_str.as_deref();

    let merge_plan = merge::plan::create_merge_plan(
        stack.forge.as_ref(),
        &stack.segments,
        &stack.repo_info,
        stack.forge_kind,
        &stack.default_branch,
        &stack.remote_name,
        &merge_options,
        stack_base,
        stack.config.stack_nav,
    )?;

    if args.watch {
        if dry_run {
            anyhow::bail!("--dry-run is not supported with --watch");
        }
        eprintln!("hint: `jjpr merge --watch` is deprecated. Use `jjpr watch` instead.\n");
        return cmd_watch(WatchArgs {
            bookmark: args.bookmark,
            preferred_remote: args.preferred_remote,
            base_override: args.base_override,
            merge_method: args.merge_method,
            required_approvals: args.required_approvals,
            ci_pass_override: args.ci_pass_override,
            reconcile_strategy: args.reconcile_strategy,
            timeout: args.timeout,
            no_fetch,
            reviewers: &[],
            reviewer_scope: jjpr::forge::types::ReviewerScope::default(),
            ready: false,
        });
    }

    if args.bookmark.is_some() {
        println!("Merging stack up to '{}'...\n", stack.target_bookmark);
    }

    let result = merge::execute::execute_merge_plan(
        &stack.jj, stack.forge.as_ref(), &merge_plan, &stack.segments, dry_run,
    )?;

    print_merge_summary(&result);
    print_local_warnings(&result, &stack.segments, stack_base, &stack.default_branch)
}

struct WatchArgs<'a> {
    bookmark: Option<&'a str>,
    preferred_remote: Option<&'a str>,
    base_override: Option<&'a str>,
    merge_method: Option<MergeMethod>,
    required_approvals: Option<u32>,
    ci_pass_override: Option<bool>,
    reconcile_strategy: Option<config::ReconcileStrategy>,
    timeout: Option<u64>,
    no_fetch: bool,
    reviewers: &'a [String],
    reviewer_scope: jjpr::forge::types::ReviewerScope,
    ready: bool,
}

fn cmd_watch(args: WatchArgs<'_>) -> Result<()> {
    let WatchArgs {
        bookmark,
        preferred_remote,
        base_override,
        merge_method,
        required_approvals,
        ci_pass_override,
        reconcile_strategy,
        timeout,
        no_fetch,
        reviewers,
        reviewer_scope,
        ready,
    } = args;
    // Compute once: gates the live in-place spinner (TTY) versus the
    // scrolling heartbeat (pipes, CI, captured output).
    let is_tty = std::io::stdout().is_terminal();
    // Set up Ctrl+C handler once, shared between bookmark wait and watch loop
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = shutdown.clone();
    ctrlc::set_handler(move || {
        eprint!("\nInterrupting after current operation completes...");
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }).expect("failed to set Ctrl+C handler");

    // Single-watcher guard: two `jjpr watch` on one repo can't corrupt anything,
    // but they double the forge API load every poll (burning the user's rate
    // limit), so exit if one is already running here. Held for the run; the
    // heartbeat is removed on drop.
    let poll_window = watch_poll_interval().as_secs().saturating_mul(2);
    let heartbeat = match jjpr::heartbeat::WatchHeartbeat::claim(&find_repo_root()?, poll_window) {
        Some(hb) => hb,
        None => {
            println!("jjpr watch is already running on this repo in another window. Exiting.");
            return Ok(());
        }
    };

    // For watch: if no bookmark is specified, try to infer one. If none exists
    // yet, wait for one to appear (unlike submit/merge which exit immediately).
    let resolved_bookmark = if let Some(name) = bookmark {
        Some(name.to_string())
    } else {
        let repo_path = find_repo_root()?;
        let jj = jjpr::jj::runner::JjRunner::new(repo_path)?;
        let graph = change_graph::build_change_graph(&jj)?;
        match analyze::infer_target_bookmark(&graph, &jj)? {
            Some(name) => Some(name),
            None => {
                let timeout_dur = timeout.map(|m| std::time::Duration::from_secs(m * 60));
                let poll = std::time::Duration::from_secs(5);
                match jjpr::watch::wait_for_bookmark(&jj, timeout_dur, poll, &shutdown, is_tty, Some(&heartbeat))? {
                    Some(name) => {
                        println!("Found bookmark '{name}'\n");
                        Some(name)
                    }
                    None => {
                        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                            println!("\nInterrupted.");
                        } else {
                            println!("Watch timed out while waiting for a bookmark.");
                        }
                        return Ok(());
                    }
                }
            }
        }
    };

    let Some(stack) = resolve_stack(resolved_bookmark.as_deref(), preferred_remote, no_fetch, "Watching", false)? else {
        return Ok(());
    };

    let merge_options = merge::plan::MergeOptions {
        merge_method: merge_method.unwrap_or(stack.config.merge_method),
        required_approvals: required_approvals.unwrap_or(stack.config.required_approvals),
        require_ci_pass: ci_pass_override.unwrap_or(stack.config.require_ci_pass),
        reconcile_strategy: reconcile_strategy.unwrap_or(stack.config.reconcile_strategy),
        ready: false,
    };

    let stack_base_str = base_override
        .map(|s| s.to_string())
        .or(stack.stack_base.clone());

    println!("Watching stack up to '{}'...\n", stack.target_bookmark);

    let submit_opts =
        jjpr::watch::WatchSubmitOptions::from_cli(reviewers.to_vec(), reviewer_scope, ready);

    let timeout_dur = timeout.map(|m| std::time::Duration::from_secs(m * 60));
    let result = jjpr::watch::run_watch_loop(
        &stack.jj,
        stack.forge.as_ref(),
        &stack.repo_info,
        stack.forge_kind,
        &stack.remote_name,
        &stack.default_branch,
        &merge_options,
        &submit_opts,
        &stack.target_bookmark,
        stack_base_str.as_deref(),
        stack.config.stack_nav,
        merge::watch::WatchOptions {
            shutdown,
            timeout: timeout_dur,
            poll_interval: watch_poll_interval(),
            is_tty,
        },
        Some(&heartbeat),
    )?;

    print_watch_summary(&result);
    print_local_warnings(
        &result.merge_result,
        &stack.segments,
        stack_base_str.as_deref(),
        &stack.default_branch,
    )
}

fn print_watch_summary(result: &jjpr::watch::WatchResult) {
    let mr = &result.merge_result;
    if !result.prs_created.is_empty() {
        let n = result.prs_created.len();
        println!("\n  Created {n} draft PR{}.", if n == 1 { "" } else { "s" });
    }
    if !result.prs_promoted.is_empty() {
        let n = result.prs_promoted.len();
        println!("  Promoted {n} PR{} to ready.", if n == 1 { "" } else { "s" });
    }

    if mr.merged.is_empty() && mr.skipped_merged.is_empty() && mr.blocked_at.is_none() {
        println!("\nWatch ended without merging anything in this stack.");
    } else if let Some(ref blocked) = mr.blocked_at {
        if blocked.reasons.iter().any(|r| matches!(r, merge::plan::BlockReason::NoPr)) {
            println!("\nRun `jjpr submit` to create PRs, then re-run `jjpr watch`.");
        } else {
            // LocalSyncFailed / ForgeReconcileFailed don't normally reach
            // here because watch keeps iterating through them. Anything
            // else here is fatal and rerunning watch is the right action.
            println!("\nRun `jjpr watch` again once the issue is resolved.");
        }
    } else if mr.merged.is_empty() && !mr.skipped_merged.is_empty() {
        println!("\nAll PRs in this stack are already merged.");
    } else {
        println!(
            "\nDone. {} PR{} merged.",
            mr.merged.len(),
            if mr.merged.len() == 1 { "" } else { "s" }
        );
    }
}

/// Watch's poll cadence. Defaults to 30s. `JJPR_WATCH_POLL_SECS` overrides it;
/// this is an undocumented test seam so the E2E parity harness can drive many
/// poll iterations in seconds instead of minutes. Not meant for end users.
fn watch_poll_interval() -> std::time::Duration {
    let secs = std::env::var("JJPR_WATCH_POLL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(30);
    std::time::Duration::from_secs(secs)
}

fn print_merge_summary(result: &merge::execute::MergeResult) {
    if result.merged.is_empty() && result.skipped_merged.is_empty() && result.blocked_at.is_none() {
        println!("\nNo PRs to merge in this stack.");
    } else if let Some(ref blocked) = result.blocked_at {
        if blocked.reasons.iter().any(|r| matches!(r, merge::plan::BlockReason::NoPr)) {
            println!("\nRun `jjpr submit` to create PRs, then re-run `jjpr watch`.");
        } else if blocked.reasons.iter().all(|r| r.is_transient()) {
            println!("\nRun `jjpr watch` to wait and auto-continue.");
        } else {
            println!("\nRun `jjpr merge` again once the issue is resolved.");
        }
    } else if result.merged.is_empty() && !result.skipped_merged.is_empty() {
        println!("\nAll PRs in this stack are already merged.");
    } else {
        println!(
            "\nDone. {} PR{} merged.",
            result.merged.len(),
            if result.merged.len() == 1 { "" } else { "s" }
        );
    }
}

fn print_local_warnings(
    result: &merge::execute::MergeResult,
    segments: &[jjpr::jj::types::NarrowedSegment],
    stack_base: Option<&str>,
    default_branch: &str,
) -> Result<()> {
    use merge::execute::DivergenceKind;

    if result.local_warnings.is_empty() {
        return Ok(());
    }

    let merged_names: std::collections::HashSet<&str> = result.merged.iter()
        .map(|m| m.bookmark_name.as_str())
        .chain(result.skipped_merged.iter().map(|s| s.bookmark_name.as_str()))
        .collect();
    let unmerged: Vec<_> = segments.iter()
        .filter(|s| !merged_names.contains(s.bookmark.name.as_str()))
        .collect();

    let local_msgs: Vec<&str> = result.local_warnings.iter()
        .filter(|w| w.kind == DivergenceKind::Local)
        .map(|w| w.message.as_str())
        .collect();
    let forge_msgs: Vec<&str> = result.local_warnings.iter()
        .filter(|w| w.kind == DivergenceKind::Forge)
        .map(|w| w.message.as_str())
        .collect();

    if !local_msgs.is_empty() {
        println!();
        println!("Note: local state is out of sync with the forge:");
        for m in &local_msgs {
            println!("  {m}");
        }
        println!();
        println!("To accept the forge state (discard local divergence):");
        println!("  jj git fetch");
        for seg in &unmerged {
            println!("  jj bookmark set {} -r {}@origin", seg.bookmark.name, seg.bookmark.name);
        }
        if let Some(first_unmerged) = unmerged.first() {
            println!();
            println!("Or to fix local state and push it to the forge:");
            let base = stack_base.unwrap_or(default_branch);
            // rebase_root: the OLDEST commit in the segment, so multi-commit
            // segments don't strand earlier commits under the old base.
            println!(
                "  jj git fetch && jj rebase -s {} -d {base}",
                merge::execute::rebase_root(first_unmerged)
            );
            println!("  # resolve any conflicts, then:");
            println!("  jjpr submit");
        }
    }

    if !forge_msgs.is_empty() {
        println!();
        println!("Note: forge reconcile failed:");
        for m in &forge_msgs {
            println!("  {m}");
        }
        println!();
        println!("Retry with `jjpr merge` (or wait for `jjpr watch` to retry).");
        println!("Persistent failures may indicate a network or forge-permission issue.");
    }

    Ok(())
}

fn cmd_config_init() -> Result<()> {
    let path = config::write_default_config()?;
    println!("Created default config at {}", path.display());
    println!("Edit it to customize merge behavior.");
    Ok(())
}

fn cmd_config_init_repo() -> Result<()> {
    let repo_path = find_repo_root()?;
    let path = config::write_repo_config(&repo_path)?;
    println!("Created repo config at {}", path.display());
    println!("Edit it to set forge type and token configuration.");
    Ok(())
}

struct ResolvedForge {
    forge: Box<dyn Forge>,
    kind: ForgeKind,
    remote_name: String,
    repo_info: RepoInfo,
}

/// Resolve the forge to use from config + remotes.
///
/// When `config.forge` is set, it's authoritative: we use that forge kind
/// and resolve the token from `config.forge_token_env` (or the forge's default
/// env var). Errors reflect the config not working, not a detection failure.
///
/// When `config.forge` is not set, we auto-detect from remote URLs.
fn resolve_forge(
    remotes: &[jjpr::jj::GitRemote],
    cfg: &config::Config,
    preferred_remote: Option<&str>,
) -> Result<ResolvedForge> {
    if let Some(kind) = cfg.forge {
        resolve_forge_from_config(remotes, kind, cfg.forge_token_env.as_deref(), preferred_remote)
    } else {
        resolve_forge_auto(remotes, preferred_remote)
    }
}

fn resolve_forge_from_config(
    remotes: &[jjpr::jj::GitRemote],
    kind: ForgeKind,
    token_env: Option<&str>,
    preferred_remote: Option<&str>,
) -> Result<ResolvedForge> {
    let env_var = token_env.unwrap_or(kind.token_env_var());
    let token = std::env::var(env_var).ok().filter(|v| !v.is_empty());

    let remote = pick_remote(remotes, preferred_remote)?;
    let host = remote::extract_host(&remote.url);
    let repo_info = remote::parse_url_as(&remote.url, kind)
        .ok_or_else(|| anyhow::anyhow!(
            "could not parse owner/repo from remote '{}' URL: {}",
            remote.name, remote.url
        ))?;

    let forge = build_forge(kind, host, token, token_env)?;
    Ok(ResolvedForge {
        forge,
        kind,
        remote_name: remote.name.clone(),
        repo_info,
    })
}

fn resolve_forge_auto(
    remotes: &[jjpr::jj::GitRemote],
    preferred_remote: Option<&str>,
) -> Result<ResolvedForge> {
    let (remote_name, kind, repo_info) = remote::resolve_remote(remotes, preferred_remote)?;
    let host = find_remote_host(remotes, &remote_name);
    let forge = build_forge(kind, host, None, None)?;
    Ok(ResolvedForge {
        forge,
        kind,
        remote_name,
        repo_info,
    })
}

fn pick_remote<'a>(
    remotes: &'a [jjpr::jj::GitRemote],
    preferred: Option<&str>,
) -> Result<&'a jjpr::jj::GitRemote> {
    if let Some(name) = preferred {
        return remotes
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| anyhow::anyhow!("remote '{}' not found", name));
    }
    if let Some(origin) = remotes.iter().find(|r| r.name == "origin") {
        return Ok(origin);
    }
    remotes
        .first()
        .ok_or_else(|| anyhow::anyhow!("no git remotes found"))
}

fn find_remote_host<'a>(remotes: &'a [jjpr::jj::GitRemote], remote_name: &str) -> Option<&'a str> {
    remotes
        .iter()
        .find(|r| r.name == remote_name)
        .and_then(|r| remote::extract_host(&r.url))
}

fn build_forge(kind: ForgeKind, host: Option<&str>, token: Option<String>, token_env: Option<&str>) -> Result<Box<dyn Forge>> {
    let token = match token {
        Some(t) => t,
        None => forge_token::resolve_token(kind, token_env)?,
    };
    match kind {
        ForgeKind::GitHub => {
            let client = ForgeClient::new("https://api.github.com", token, AuthScheme::Bearer, PaginationStyle::LinkHeader);
            Ok(Box::new(GitHubForge::new(client)))
        }
        ForgeKind::GitLab => {
            let gitlab_host = host.unwrap_or("gitlab.com");
            let base_url = format!("https://{gitlab_host}/api/v4");
            let client = ForgeClient::new(&base_url, token, AuthScheme::Bearer, PaginationStyle::LinkHeader);
            Ok(Box::new(GitLabForge::new(client)))
        }
        ForgeKind::Forgejo => {
            let host = host.ok_or_else(|| anyhow::anyhow!("could not determine Forgejo host from remote URL"))?;
            let base_url = format!("https://{host}/api/v1");
            let client = ForgeClient::new(&base_url, token, AuthScheme::Token, PaginationStyle::PageNumber { limit: 50 });
            Ok(Box::new(ForgejoForge::new(client)))
        }
    }
}

fn print_forge_detection(detected: &DetectedForge) {
    let source = match &detected.source {
        ForgeSource::Config => "from config".to_string(),
        ForgeSource::Remote(name) => format!("from remote '{name}'"),
    };
    println!("Detected forge: {} ({source})", detected.kind);
}

struct DetectedForge {
    kind: ForgeKind,
    host: Option<String>,
    token: Option<String>,
    /// The env var name used to resolve the token (for error messages)
    token_env_var: Option<String>,
    source: ForgeSource,
}

enum ForgeSource {
    Config,
    Remote(String),
}

/// Best-effort forge detection for auth commands.
/// Checks repo-local config first; falls back to auto-detection from remotes.
fn detect_forge_for_cwd() -> Option<DetectedForge> {
    let repo_path = find_repo_root().ok()?;
    let cfg = config::load_config_with_repo(Some(&repo_path)).ok()?;
    let jj = JjRunner::new(repo_path).ok()?;
    let remotes = jj.get_git_remotes().ok()?;

    if let Some(kind) = cfg.forge {
        let host = pick_remote(&remotes, None)
            .ok()
            .and_then(|r| remote::extract_host(&r.url).map(|s| s.to_string()));
        let env_var = cfg.forge_token_env.as_deref().unwrap_or(kind.token_env_var());
        let token = std::env::var(env_var).ok();
        return Some(DetectedForge {
            kind,
            host,
            token,
            token_env_var: Some(env_var.to_string()),
            source: ForgeSource::Config,
        });
    }

    let (remote_name, kind, _) = remote::resolve_remote(&remotes, None).ok()?;
    let host = find_remote_host(&remotes, &remote_name).map(|s| s.to_string());
    Some(DetectedForge { kind, host, token: None, token_env_var: None, source: ForgeSource::Remote(remote_name) })
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
            None => {
                // Check if there's a git repo that could be colocated
                let mut check = cwd.as_path();
                loop {
                    if check.join(".git").exists() {
                        anyhow::bail!(
                            "found a git repository but no jj repository. \
                             Run `jj git init --colocate` to set up jj alongside git."
                        );
                    }
                    match check.parent() {
                        Some(parent) => check = parent,
                        None => break,
                    }
                }
                anyhow::bail!(
                    "not a jj repository (or any parent up to /). \
                     Run `jj git init` to create one."
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jjpr::forge::types::PrMergeability;

    fn bookmark(has_remote: bool, is_synced: bool) -> Bookmark {
        Bookmark {
            name: "b".to_string(),
            commit_id: "c".to_string(),
            change_id: "z".to_string(),
            has_remote,
            is_synced,
        }
    }

    #[test]
    fn sync_status_all_synced_is_up_to_date() {
        let bms = [bookmark(true, true), bookmark(true, true)];
        assert_eq!(sync_status_label(&bms), "push up to date");
    }

    #[test]
    fn sync_status_pushed_but_ahead_needs_updating() {
        // Pushed at least once (has_remote) but the remote no longer matches.
        let bms = [bookmark(true, false)];
        assert_eq!(sync_status_label(&bms), "push needs updating");
    }

    #[test]
    fn sync_status_never_pushed_is_not_pushed_yet() {
        let bms = [bookmark(false, false)];
        assert_eq!(sync_status_label(&bms), "not pushed yet");
    }

    #[test]
    fn sync_status_mixed_synced_and_ahead_needs_updating() {
        // One bookmark is up to date, another has unpushed changes: the
        // segment as a whole still needs a push.
        let bms = [bookmark(true, true), bookmark(true, false)];
        assert_eq!(sync_status_label(&bms), "push needs updating");
    }

    #[test]
    fn sync_status_empty_is_not_pushed_yet() {
        assert_eq!(sync_status_label(&[]), "not pushed yet");
    }

    #[test]
    fn status_line_renders_mergeability_checks_and_reviews() {
        let status = SegmentDisplayStatus {
            mergeability: Some(PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".to_string(),
            }),
            checks: Some(ChecksStatus::Pass),
            reviews: Some(ReviewSummary { approved_count: 1, changes_requested: false }),
        };
        let line = format_status_line(&status);
        assert!(line.contains("mergeable"), "got: {line}");
        assert!(line.contains("CI passing"), "got: {line}");
        assert!(line.contains("1 approval"), "got: {line}");
    }

    #[test]
    fn status_line_empty_when_no_signals() {
        let status = SegmentDisplayStatus { mergeability: None, checks: None, reviews: None };
        assert_eq!(format_status_line(&status), "");
    }
}
