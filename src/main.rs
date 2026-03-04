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
use jjpr::forge::types::{MergeMethod, PullRequest, RepoInfo};
use jjpr::forge::{AuthScheme, Forge, ForgeClient, ForgejoForge, ForgeKind, GitHubForge, GitLabForge, PaginationStyle};
use jjpr::forge::token as forge_token;
use jjpr::graph::change_graph;
use jjpr::jj::{Jj, JjRunner};
use jjpr::merge;
use jjpr::submit::{analyze, execute, plan, resolve};

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
    let jj = JjRunner::new(repo_path.clone())?;
    let cfg = config::load_config_with_repo(Some(&repo_path))?;

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
    let resolved = resolve_forge(&remotes, &cfg, opts.preferred_remote)?;
    let ResolvedForge { forge, kind: forge_kind, remote_name, repo_info } = resolved;

    let default_branch = jj.get_default_branch()?;

    let graph = change_graph::build_change_graph(&jj)?;

    let analysis = analyze::analyze_submission_graph(&graph, &target_bookmark)?;

    let interactive = std::io::stdout().is_terminal();
    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, interactive)?;

    let stack_base = opts.base_override.or(analysis.base_branch.as_deref());
    let submission_plan = plan::create_submission_plan(
        forge.as_ref(),
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
    execute::execute_submission_plan(&jj, forge.as_ref(), &submission_plan, opts.reviewers, opts.dry_run)?;
    println!("\nDone.");

    Ok(())
}

fn cmd_stack_overview(no_fetch: bool) -> Result<()> {
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

    // Try to resolve forge remote for PR info
    let (forge_kind, pr_info) = try_load_pr_info(&jj, &cfg, &graph)
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
        }
        if let Some(base) = &stack.base_branch {
            println!("  (based on {base})");
        }
    }

    Ok(())
}

fn try_load_pr_info(
    jj: &dyn Jj,
    cfg: &config::Config,
    graph: &change_graph::ChangeGraph,
) -> Option<(ForgeKind, HashMap<String, PullRequest>)> {
    let remotes = jj.get_git_remotes().ok()?;
    let resolved = resolve_forge(&remotes, cfg, None).ok()?;
    let ResolvedForge { forge, kind, repo_info, .. } = resolved;

    let all_prs = match forge.list_open_prs(&repo_info.owner, &repo_info.repo) {
        Ok(prs) => prs,
        Err(_) => {
            if !graph.stacks.is_empty() && forge.get_authenticated_user().is_err() {
                eprintln!("hint: run `jjpr auth test` to check authentication for stack overview");
            }
            return Some((kind, HashMap::new()));
        }
    };

    Some((kind, jjpr::forge::build_pr_map(all_prs, &repo_info.owner)))
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
    let jj = JjRunner::new(repo_path.clone())?;
    let cfg = config::load_config_with_repo(Some(&repo_path))?;

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
    let resolved = resolve_forge(&remotes, &cfg, args.preferred_remote)?;
    let ResolvedForge { forge, kind: forge_kind, remote_name, repo_info } = resolved;

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
        forge.as_ref(),
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
        &jj, forge.as_ref(), &merge_plan, &segments, dry_run,
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
