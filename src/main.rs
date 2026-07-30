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

use std::collections::{HashMap, HashSet};

use jjpr::cli::{AuthCommands, Cli, Commands, ConfigCommands};
use jjpr::config;
use jjpr::forge::remote;
use jjpr::forge::types::{ChecksStatus, MergeMethod, PullRequest, RepoInfo};
use jjpr::forge::{AuthScheme, Forge, ForgeClient, ForgejoForge, ForgeKind, GitHubForge, GitLabForge, PaginationStyle};
use jjpr::forge::token as forge_token;
use jjpr::forge::status as forge_status;
use jjpr::parallel;
use jjpr::graph::change_graph;
use jjpr::identity::Identity;
use jjpr::jj::types::{Bookmark, BookmarkSegment, BranchStack};
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
    let mut jj = JjRunner::new(repo_path.clone())?;
    // jjpr is otherwise working-copy-agnostic; for user-invoked commands
    // (submit/merge) snapshot once so we act on the user's latest edits. The
    // autonomous watch loop passes false — it operates on committed state.
    if snapshot {
        jj.snapshot()?;
    }
    let cfg = config::load_config_with_repo(Some(&repo_path))?;

    // Recognize work authored under any of your emails (local + configured), so
    // discovery isn't blind to a branch you wrote under a different machine's
    // email. Free and no-network; the forge's verified emails are added lazily
    // (Tier 2) below only if this seed can't find your work.
    let local_email = jj.get_user_email().unwrap_or_default();
    let mut identity = Identity::seed(&local_email, &cfg.identity.emails, &cfg.identity.logins);
    jj.set_identity(&identity);

    if !no_fetch {
        eprintln!("Fetching remotes...");
        jj.git_fetch()?;
    }

    // Resolve the forge up front so Tier 2 can consult it, but tolerate its
    // absence: the no-bookmark case below should still report "no bookmark",
    // not a forge error.
    let remotes = jj.get_git_remotes()?;
    let forge_result = resolve_forge(&remotes, &cfg, preferred_remote);

    let target_bookmark = match bookmark {
        Some(name) => name.to_string(),
        None => {
            let graph = change_graph::build_change_graph(&jj)?;
            let mut inferred = analyze::infer_target_bookmark(&graph, &jj)?;
            // Tier 2: nothing of yours under the working copy, but there IS a
            // bookmark there — your work may be under an email we don't know.
            // Fetch the account's verified emails and try once more.
            let unowned_bookmark = inferred.is_none() && working_copy_has_bookmark(&jj);
            if unowned_bookmark
                && let Ok(resolved) = &forge_result
                && augment_identity_from_forge(&mut jj, resolved.forge.as_ref(), &mut identity)
            {
                let graph = change_graph::build_change_graph(&jj)?;
                inferred = analyze::infer_target_bookmark(&graph, &jj)?;
            }
            match inferred {
                Some(inferred) => {
                    println!("{command_verb} stack for '{inferred}' (inferred from working copy)\n");
                    inferred
                }
                // A bookmark is present but authored under an email jjpr doesn't
                // recognize as yours (and it couldn't confirm via the forge).
                None if unowned_bookmark => {
                    println!("A bookmark in the working copy isn't recognized as yours —");
                    println!("likely authored under a different email. Add it with");
                    println!("`[identity] emails = [\"...\"]` in the jjpr config, or name it explicitly.");
                    return Ok(None);
                }
                None => {
                    println!("No bookmark found in the working copy's ancestry.");
                    println!("Set a bookmark with `jj bookmark set <name>` or specify one: `jjpr <command> <bookmark>`");
                    return Ok(None);
                }
            }
        }
    };

    let ResolvedForge { forge, kind: forge_kind, remote_name, repo_info } = forge_result?;

    let default_branch = jj.get_default_branch()?;
    let mut graph = change_graph::build_change_graph(&jj)?;
    let analysis = match analyze::analyze_submission_graph(&graph, &target_bookmark) {
        Ok(analysis) => analysis,
        // Tier 2 for an explicit bookmark authored under an unknown email.
        Err(err) => {
            if augment_identity_from_forge(&mut jj, forge.as_ref(), &mut identity) {
                graph = change_graph::build_change_graph(&jj)?;
                analyze::analyze_submission_graph(&graph, &target_bookmark)?
            } else {
                return Err(err);
            }
        }
    };
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

/// Tier 2: fetch the account's verified emails and fold them into `identity`,
/// re-applying to `jj`. Returns whether it added anything (caller rebuilds).
/// Best-effort — a token without the email scope simply doesn't augment.
fn augment_identity_from_forge(
    jj: &mut JjRunner,
    forge: &dyn Forge,
    identity: &mut Identity,
) -> bool {
    let Ok(emails) = forge.get_authenticated_emails() else {
        return false;
    };
    let before = identity.emails.len();
    identity.extend_emails(emails);
    let grew = identity.emails.len() > before;
    if grew {
        jj.set_identity(identity);
    }
    grew
}

/// Whether the working copy sits on any bookmarked stack, regardless of author.
/// Gates the Tier 2 forge call: only reach for `/user/emails` when there's work
/// we might be failing to recognize as yours.
fn working_copy_has_bookmark(jj: &dyn Jj) -> bool {
    change_graph::build_status_graph(jj, false).map(|g| !g.stacks.is_empty()).unwrap_or(false)
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
    let mut jj = JjRunner::new(repo_path.clone())?;
    let cfg = config::load_config_with_repo(Some(&repo_path))?;

    if !no_fetch {
        eprintln!("Fetching remotes...");
    }

    // Read the remotes before anything else touches jj concurrently.
    let remotes = jj.get_git_remotes().unwrap_or_default();

    // `jj git fetch` is a second of network latency that the forge lookups do
    // not depend on, so run them underneath it. The graph does depend on the
    // fetch (it decides where trunk() is), so the fetch is joined before any of
    // it is built.
    //
    // Nothing in this scope may touch jj: two jj processes on one repo contend
    // for the op-log lock, which is why the remotes are read above and the
    // identity seeding below waits until the scope has closed.
    let (fetch_result, info) = std::thread::scope(|scope| {
        let fetch = (!no_fetch).then(|| scope.spawn(|| jj.git_fetch()));
        let info = load_pr_info(&remotes, &cfg).unwrap_or_default();
        let fetch_result = fetch.map(|handle| handle.join().expect("fetch thread panicked"));
        (fetch_result, info)
    });
    if let Some(result) = fetch_result {
        result?;
    }

    // Seed the identities that count as you (local email + config), free and
    // no-network, so discovery recognizes work authored under a configured
    // second email. A forge login is added lazily below if it could matter.
    let mut identity = jjpr::identity::Identity::seed(
        &jj.get_user_email().unwrap_or_default(),
        &cfg.identity.emails,
        &cfg.identity.logins,
    );
    jj.set_identity(&identity);

    // The bare view infers from the working copy, so it only needs `@`'s
    // ancestry. A positional bookmark or `--all` must also find your other
    // stacks by name, which costs the larger `::mine()` closure.
    let graph = change_graph::build_status_graph(&jj, all || bookmark.is_some())?;

    if graph.stacks.is_empty() {
        println!("No stacks found. Create bookmarks with `jj bookmark set <name>`.");
        return Ok(());
    }

    // Author-scoped bookmarks (mine()) classify each segment: yours (jjpr acts
    // on it) vs. someone else's (display-only). Discovery above is agnostic.
    // Propagate a failure rather than fall back to an empty set — an empty set
    // would mislabel your own stack as entirely someone else's.
    let my_names: HashSet<String> =
        jj.get_my_bookmarks()?.into_iter().map(|b| b.name).collect();
    let segment_is_mine =
        |seg: &BookmarkSegment| seg.bookmarks.iter().any(|b| my_names.contains(&b.name));

    let stacks_to_show = match analyze::select_stacks_to_show(&graph, bookmark, all, &jj, &my_names)? {
        analyze::StackScope::Show(stacks) => stacks,
        analyze::StackScope::NoTarget => {
            println!("No bookmark in working copy ancestry.");
            println!("Use `jjpr status --all` to see your stacks, or `jj bookmark set <name>` to mark one.");
            return Ok(());
        }
        analyze::StackScope::Unknown(name) => {
            println!("Bookmark '{name}' not found in any stack containing your work.");
            println!("Run `jjpr status --all` to see your stacks.");
            return Ok(());
        }
    };

    // The PR list ran concurrently with the fetch above. Its failure only earns
    // an auth hint once the graph shows there is a stack to report on.
    if let Some(forge) = &info.failed_forge
        && !graph.stacks.is_empty()
        && forge.get_authenticated_user().is_err()
    {
        eprintln!("hint: run `jjpr auth test` to check authentication for stack overview");
    }

    // Per-segment forge state. list_open_prs returns everyone's open PRs in the
    // repo, so a coworker's open base lands in pr_map keyed by branch name. A
    // merged PR is looked up (by head branch) only for SOMEONE ELSE'S segment —
    // never yours: `find_merged_pr` matches by branch name alone, so a fresh
    // bookmark reusing an old, since-merged branch name would otherwise be
    // mislabeled "merged, clean up" over live unpushed work.
    let mut status_map: HashMap<String, SegmentDisplayStatus> = HashMap::new();
    let mut merged_map: HashMap<String, PullRequest> = HashMap::new();
    if let (Some(forge), Some(repo_info)) = (&info.forge, &info.repo_info) {
        // Decide every call up front, then issue them together. Each lookup is
        // independent, so walking the stack and blocking on each in turn made
        // latency scale with the stack's height for no reason.
        let mut status_targets: Vec<(String, PullRequest)> = Vec::new();
        let mut merged_lookups: Vec<String> = Vec::new();
        for stack in &stacks_to_show {
            for segment in &stack.segments {
                let Some(bookmark) = segment.bookmarks.first() else {
                    continue;
                };
                if let Some(pr) = info.pr_map.get(&bookmark.name) {
                    // A diamond puts the shared segments in more than one stack;
                    // without this the same PR is queried once per stack it
                    // appears in.
                    if !status_targets.iter().any(|(name, _)| name == &bookmark.name) {
                        status_targets.push((bookmark.name.clone(), pr.clone()));
                    }
                } else if !segment_is_mine(segment) && !merged_lookups.contains(&bookmark.name) {
                    merged_lookups.push(bookmark.name.clone());
                }
            }
            // A foreign base (a coworker's branch you rebased onto with no local
            // bookmark) is a branch name, not a segment. Look up its merged PR so
            // the `(based on X)` footer can be attributed too; its open PR is
            // already in pr_map.
            if let Some(base) = &stack.base_branch
                && !info.pr_map.contains_key(base)
                && !merged_lookups.contains(base)
            {
                merged_lookups.push(base.clone());
            }
        }

        status_map = fetch_all_segment_status(forge.as_ref(), repo_info, &status_targets);

        let merged = parallel::map_bounded(
            &merged_lookups,
            parallel::MAX_CONCURRENT_REQUESTS,
            |name| forge.find_merged_pr(&repo_info.owner, &repo_info.repo, name),
        );
        for (name, result) in merged_lookups.iter().zip(merged) {
            if let Ok(Some(pr)) = result {
                merged_map.insert(name.clone(), pr);
            }
        }
    }

    // Tier 1 (lazy): if a segment is someone else's by email but carries a PR,
    // that PR's author login can reveal it's actually yours (same forge account,
    // a different machine's commit email). Fetch your login once — only when it
    // could change a classification — and never touch `/user/emails` for this.
    if let Some(forge) = &info.forge {
        let could_reclassify = stacks_to_show.iter().flat_map(|s| &s.segments).any(|seg| {
            !segment_is_mine(seg)
                && seg.bookmarks.first().is_some_and(|b| {
                    info.pr_map.contains_key(&b.name) || merged_map.contains_key(&b.name)
                })
        });
        if could_reclassify
            && let Ok(login) = forge.get_authenticated_user()
        {
            identity.add_login(&login);
        }
    }

    // Forward-looking dismiss-stale note (Feature 2): each of your approved open
    // PRs stacked atop another open PR loses its approvals when the lower PR is
    // squash-landed (the reconcile rebases-and-force-pushes it). Map every
    // segment to the PR directly below it, then — only when an approval is
    // actually at risk — spend one call to learn whether trunk resets approvals.
    let mut below_pr: HashMap<String, u64> = HashMap::new();
    for stack in &stacks_to_show {
        for pair in stack.segments.windows(2) {
            let (Some(upper), Some(lower)) = (pair[0].bookmarks.first(), pair[1].bookmarks.first())
            else {
                continue;
            };
            if let Some(lower_pr) = info.pr_map.get(&lower.name) {
                below_pr.entry(upper.name.clone()).or_insert(lower_pr.number);
            }
        }
    }
    let trunk_dismisses_stale = match (&info.forge, &info.repo_info) {
        (Some(forge), Some(repo_info))
            if below_pr.keys().any(|name| {
                status_map
                    .get(name)
                    .and_then(|s| s.reviews.as_ref())
                    .is_some_and(|r| r.approved_count > 0)
            }) =>
        {
            match jj.get_default_branch() {
                Ok(trunk) if !trunk.is_empty() => forge
                    .base_dismisses_stale_approvals(&repo_info.owner, &repo_info.repo, &trunk)
                    .ok()
                    .flatten(),
                _ => None,
            }
        }
        _ => None,
    };

    let render = StatusRender {
        pr_map: &info.pr_map,
        merged_map: &merged_map,
        status_map: &status_map,
        my_names: &my_names,
        identity: Some(&identity),
        has_forge: info.forge.is_some(),
        trunk_dismisses_stale,
        below_pr,
        forge_kind: info.forge_kind,
    };

    let multi = stacks_to_show.len() > 1;
    for (i, stack) in stacks_to_show.iter().enumerate() {
        if i > 0 {
            println!();
        }
        // A stack you're viewing that is entirely someone else's (you're sitting
        // on their branch): recognize it rather than render a stack you can't
        // act on. Only for the scoped views — `--all` renders uniformly.
        if !all && render.stack_is_all_foreign(stack) {
            for line in render.render_foreign_only(stack) {
                println!("{line}");
            }
            continue;
        }
        if multi {
            println!("Stack {}:", i + 1);
        }
        for segment in &stack.segments {
            for line in render.render_segment(segment) {
                println!("{line}");
            }
        }
        if let Some(base) = &stack.base_branch {
            for line in render.render_base(base) {
                println!("{line}");
            }
        }
    }

    Ok(())
}

#[derive(Default)]
struct PrInfoResult {
    pr_map: HashMap<String, PullRequest>,
    forge: Option<Box<dyn Forge>>,
    forge_kind: Option<ForgeKind>,
    repo_info: Option<RepoInfo>,
    /// Set when the PR list call failed.
    ///
    /// The forge lands here rather than in `forge` so the rest of the command
    /// behaves exactly as it does with no forge at all, while the auth hint can
    /// still probe once the graph has shown there are stacks worth hinting
    /// about. Probing here instead would spend a request on every repo that has
    /// no stacks.
    failed_forge: Option<Box<dyn Forge>>,
}

/// Resolve the forge and pull the repo's open PRs.
///
/// Deliberately free of any `jj` access: the caller runs this concurrently with
/// `jj git fetch`, and a second jj process on the same repo would contend for
/// the op-log lock. Take the remotes before calling.
fn load_pr_info(remotes: &[jjpr::jj::GitRemote], cfg: &config::Config) -> Option<PrInfoResult> {
    let resolved = resolve_forge(remotes, cfg, None).ok()?;
    let ResolvedForge { forge, repo_info, kind, .. } = resolved;

    let all_prs = match forge.list_open_prs(&repo_info.owner, &repo_info.repo) {
        Ok(prs) => prs,
        Err(_) => {
            return Some(PrInfoResult {
                failed_forge: Some(forge),
                ..Default::default()
            });
        }
    };

    let pr_map = jjpr::forge::build_pr_map(all_prs, &repo_info.owner);
    Some(PrInfoResult {
        pr_map,
        forge: Some(forge),
        forge_kind: Some(kind),
        repo_info: Some(repo_info),
        failed_forge: None,
    })
}

/// The same shape the forge layer returns from a batch lookup; kept as an alias
/// so the batched and per-PR paths cannot drift apart.
type SegmentDisplayStatus = jjpr::forge::PrStatusBundle;

/// Resolve display status for every segment that has an open PR, keyed by the
/// bookmark the render looks it up by.
fn fetch_all_segment_status(
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    targets: &[(String, PullRequest)],
) -> HashMap<String, SegmentDisplayStatus> {
    let prs: Vec<&PullRequest> = targets.iter().map(|(_, pr)| pr).collect();
    let by_number = forge_status::fetch_all(forge, repo_info, &prs);
    targets
        .iter()
        .filter_map(|(name, pr)| {
            by_number
                .get(&pr.number)
                .map(|status| (name.clone(), status.clone()))
        })
        .collect()
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

/// The mergeability half of a status line only — what's relevant for a
/// coworker's base (you want to know when it can/does land, not its CI/reviews).
fn format_mergeability_line(status: &SegmentDisplayStatus) -> String {
    match status.mergeability.as_ref().and_then(|m| m.mergeable) {
        Some(true) => "    \u{2713} mergeable".to_string(),
        Some(false) => "    \u{2717} conflicts".to_string(),
        None => String::new(),
    }
}

/// A segment's PR as it matters for display: an open/draft PR, a merged PR
/// (found by branch after it left the open list), or none.
enum SegmentPr<'a> {
    Open(&'a PullRequest),
    Merged(&'a PullRequest),
    None,
}

/// Everything the status renderer needs, borrowed for the render pass.
struct StatusRender<'a> {
    pr_map: &'a HashMap<String, PullRequest>,
    merged_map: &'a HashMap<String, PullRequest>,
    status_map: &'a HashMap<String, SegmentDisplayStatus>,
    /// Bookmarks on commits by any of your emails. A segment is "yours" iff one
    /// of its bookmarks is here — exactly what submit/watch/merge act on.
    my_names: &'a HashSet<String>,
    /// Your identities, for the display-only login supplement: a PR authored by
    /// your forge account is yours even when its commit email isn't known.
    /// `None` disables the supplement (email classification only).
    identity: Option<&'a Identity>,
    has_forge: bool,
    /// Whether trunk resets approvals on push. `Some(true)` means a squash
    /// landing of a lower PR rebases-and-force-pushes its descendants, dismissing
    /// their standing approvals. `None` when undetermined or nothing at risk.
    trunk_dismisses_stale: Option<bool>,
    /// Leading-bookmark name → PR number of the segment directly below it, when
    /// that segment has an open PR. The lower PR whose squash-landing would
    /// rebase this one; used only to name it in the dismiss-stale note.
    below_pr: HashMap<String, u64>,
    /// For formatting the lower PR reference (`#123` vs `!123`).
    forge_kind: Option<ForgeKind>,
}

impl StatusRender<'_> {
    fn resolve_pr(&self, name: &str) -> SegmentPr<'_> {
        if let Some(pr) = self.pr_map.get(name) {
            SegmentPr::Open(pr)
        } else if let Some(pr) = self.merged_map.get(name) {
            SegmentPr::Merged(pr)
        } else {
            SegmentPr::None
        }
    }

    fn segment_is_mine(&self, segment: &BookmarkSegment) -> bool {
        segment.bookmarks.iter().any(|b| self.my_names.contains(&b.name))
    }

    /// "Yours" for display = mine by email, OR (login supplement) the segment's
    /// PR was authored by your forge account.
    fn segment_is_yours(&self, segment: &BookmarkSegment) -> bool {
        if self.segment_is_mine(segment) {
            return true;
        }
        let Some(identity) = self.identity else {
            return false;
        };
        segment.bookmarks.first().is_some_and(|b| match self.resolve_pr(&b.name) {
            SegmentPr::Open(p) | SegmentPr::Merged(p) => identity.owns_login(&p.author),
            SegmentPr::None => false,
        })
    }

    fn stack_is_all_foreign(&self, stack: &BranchStack) -> bool {
        !stack.segments.is_empty() && stack.segments.iter().all(|s| !self.segment_is_yours(s))
    }

    /// The parenthesized third field. For yours it's the push/sync state; for a
    /// coworker's still-open PR it says jjpr leaves it alone; a merged/no-PR
    /// foreign segment has nothing to add.
    fn header_slot(&self, pr: &SegmentPr, mine: bool, bookmarks: &[Bookmark]) -> Option<String> {
        if matches!(pr, SegmentPr::Merged(_)) {
            None // a merged branch's push/sync state is moot
        } else if mine {
            Some(sync_status_label(bookmarks).to_string())
        } else if matches!(pr, SegmentPr::Open(_)) {
            Some("jjpr won't submit or merge it".to_string())
        } else {
            None
        }
    }

    fn render_segment(&self, segment: &BookmarkSegment) -> Vec<String> {
        let name = segment.bookmarks.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ");
        let count = segment.changes.len();
        let plural = if count == 1 { "" } else { "s" };
        let merge_label = if segment.merge_source_names.is_empty() {
            String::new()
        } else {
            format!(", merge of {}", segment.merge_source_names.join(" + "))
        };

        let mine = self.segment_is_yours(segment);
        let pr = segment
            .bookmarks
            .first()
            .map_or(SegmentPr::None, |b| self.resolve_pr(&b.name));

        let pr_label = pr_label(&pr, mine);
        let slot = self
            .header_slot(&pr, mine, &segment.bookmarks)
            .map(|s| format!(", {s}"))
            .unwrap_or_default();

        let mut lines = vec![format!("  {name} ({count} change{plural}{merge_label}{pr_label}{slot})")];
        lines.extend(self.render_detail(segment, &pr, mine));
        lines
    }

    /// The forward-looking dismiss-stale note for one of your open, approved PRs
    /// that sits atop another open PR: a squash landing of the lower PR rebases
    /// and force-pushes this one, dismissing its approvals. Only speculative
    /// wording ("would") — a merge-commit landing skips the rebase (see
    /// `is_rooted_in`), so it's genuinely conditional on how the lower PR lands.
    fn dismiss_stale_note(&self, name: &str, mine: bool) -> Option<String> {
        if !mine || self.trunk_dismisses_stale != Some(true) {
            return None;
        }
        let below = self.below_pr.get(name)?;
        let n = self.status_map.get(name).and_then(|s| s.reviews.as_ref())?.approved_count;
        if n == 0 {
            return None;
        }
        let reference = self
            .forge_kind
            .map_or_else(|| format!("#{below}"), |k| k.format_ref(*below));
        Some(format!(
            "    \u{26a0} a squash-landing of {reference} would dismiss {n} approval{}",
            if n == 1 { "" } else { "s" }
        ))
    }

    /// Note that a PR belongs to a GitHub native stack, which jjpr cannot merge.
    ///
    /// Without this, `status` reports `✓ mergeable` for a PR that `jjpr merge`
    /// refuses outright — two commands giving opposite answers about the same
    /// PR. Read from the `stack` object GitHub embeds in the PR payload jjpr
    /// already fetches, so it costs no extra request.
    ///
    /// Only for PRs jjpr would otherwise merge: a foreign segment already says
    /// jjpr won't merge it, and repeating the reason there is noise.
    /// Every field of `PrStackRef` is defaulted, so a payload that stopped
    /// carrying one yields 0 rather than failing to parse. Say less in that
    /// case instead of printing "#0" or "(0 of 0)": the membership itself is
    /// the load-bearing fact, and it is still true.
    fn native_stack_note(pr: &PullRequest, mine: bool) -> Option<String> {
        if !mine {
            return None;
        }
        let stack = pr.stack.as_ref()?;
        let which = if stack.number > 0 {
            format!("native stack #{}", stack.number)
        } else {
            "a native stack".to_string()
        };
        let position = if stack.position > 0 && stack.size > 0 {
            format!(" ({} of {})", stack.position, stack.size)
        } else {
            String::new()
        };
        // Say what the command lands. Merging a stacked PR lands everything
        // below it, which is the semantic most likely to surprise, and the merge
        // block reason already spells it out — status should not be vaguer about
        // the same command.
        let lands = match stack.position {
            0 => format!("use `gh stack merge {}`", pr.number),
            1 => format!("`gh stack merge {}` lands it", pr.number),
            n => format!("`gh stack merge {}` lands it and the {} below", pr.number, n - 1),
        };
        Some(format!(
            "    \u{26a0} in {which}{position}, so jjpr cannot merge it; {lands}"
        ))
    }

    fn render_detail(&self, segment: &BookmarkSegment, pr: &SegmentPr, mine: bool) -> Vec<String> {
        let Some(bookmark) = segment.bookmarks.first() else {
            return vec![];
        };
        match pr {
            SegmentPr::Open(p) => {
                let mut lines = vec![format!("    {}", p.html_url)];
                let detail = if mine {
                    if p.draft {
                        String::new()
                    } else {
                        self.status_map.get(&bookmark.name).map(format_status_line).unwrap_or_default()
                    }
                } else {
                    self.status_map.get(&bookmark.name).map(format_mergeability_line).unwrap_or_default()
                };
                if !detail.is_empty() {
                    lines.push(detail);
                }
                // Drafts hide review detail, so they hide the review-loss note too.
                if !p.draft && let Some(note) = self.dismiss_stale_note(&bookmark.name, mine) {
                    lines.push(note);
                }
                // Shown for drafts too: it is not review detail, and a draft in a
                // native stack is just as unmergeable by jjpr.
                if let Some(note) = Self::native_stack_note(p, mine) {
                    lines.push(note);
                }
                lines
            }
            SegmentPr::Merged(p) => {
                let when = merged_on_suffix(p);
                let mut lines = vec![format!("    {}", p.html_url)];
                if bookmark.has_remote {
                    lines.push(format!("    \u{2713} merged{when}"));
                } else {
                    lines.push(format!(
                        "    \u{2713} merged{when}; remote branch deleted, local bookmark is stale"
                    ));
                    lines.push(format!("       clean up: jj bookmark forget {}", bookmark.name));
                }
                lines
            }
            SegmentPr::None => {
                if mine && self.has_forge {
                    vec!["    no PR yet — run `jjpr submit`".to_string()]
                } else {
                    vec![]
                }
            }
        }
    }

    /// The `(based on X)` footer, enriched when X has a PR. A base is a branch
    /// you're stacked on but don't own locally (often a coworker's remote
    /// branch with no local bookmark), so it's always attributed.
    fn render_base(&self, base: &str) -> Vec<String> {
        match self.resolve_pr(base) {
            SegmentPr::Open(p) => vec![
                format!("  (based on {base} \u{2014} PR open{})", author_suffix(p, false)),
                format!("    {}", p.html_url),
            ],
            SegmentPr::Merged(p) => vec![
                format!("  (based on {base} \u{2014} PR merged{})", author_suffix(p, false)),
                format!("    {}", p.html_url),
            ],
            SegmentPr::None => vec![format!("  (based on {base})")],
        }
    }

    /// Scenario where the working copy sits on a stack that's entirely someone
    /// else's: name the branch, show it, and say there's nothing to submit.
    fn render_foreign_only(&self, stack: &BranchStack) -> Vec<String> {
        let leaf = stack.segments.first().and_then(|s| s.bookmarks.first());
        let leaf_name = leaf.map_or("this stack", |b| b.name.as_str());
        let merged = leaf.is_some_and(|b| matches!(self.resolve_pr(&b.name), SegmentPr::Merged(_)));
        let qualifier = if merged {
            "someone else's merged branch"
        } else {
            "someone else's branch"
        };
        let mut lines = vec![format!("On {leaf_name} \u{2014} {qualifier}:"), String::new()];
        for segment in &stack.segments {
            lines.extend(self.render_segment(segment));
        }
        if let Some(base) = &stack.base_branch {
            lines.extend(self.render_base(base));
        }
        lines.push(String::new());
        lines.push("Nothing of yours to submit here.".to_string());
        lines
    }
}

/// ` by @author` for someone else's PR, empty for yours (or when the forge
/// omits the author).
fn author_suffix(pr: &PullRequest, mine: bool) -> String {
    if mine {
        String::new()
    } else if pr.author.is_empty() {
        " by someone else".to_string()
    } else {
        format!(" by @{}", pr.author)
    }
}

/// The `PR open/draft/merged [by @author]` label. Author attribution appears
/// only for someone else's PR.
fn pr_label(pr: &SegmentPr, mine: bool) -> String {
    match pr {
        SegmentPr::Open(p) if p.draft => format!(", PR draft{}", author_suffix(p, mine)),
        SegmentPr::Open(p) => format!(", PR open{}", author_suffix(p, mine)),
        SegmentPr::Merged(p) => format!(", PR merged{}", author_suffix(p, mine)),
        SegmentPr::None => String::new(),
    }
}

/// " on YYYY-MM-DD" from an ISO merged-at timestamp, or empty. Uses a checked
/// slice so a malformed/non-ASCII timestamp degrades instead of panicking.
fn merged_on_suffix(pr: &PullRequest) -> String {
    match pr.merged_at.as_deref().and_then(|s| s.get(..10)) {
        Some(date) => format!(" on {date}"),
        None => String::new(),
    }
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
        let mut jj = jjpr::jj::runner::JjRunner::new(repo_path.clone())?;
        // Seed identities (local + config) so watch's bookmark inference and the
        // wait-for-bookmark loop recognize work under a configured email —
        // matching resolve_stack, which seeds the jj used for the watch loop.
        let cfg = config::load_config_with_repo(Some(&repo_path))?;
        let local_email = jj.get_user_email().unwrap_or_default();
        jj.set_identity(&Identity::seed(&local_email, &cfg.identity.emails, &cfg.identity.logins));
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
        println!("{}", blocked_follow_up(&blocked.reasons, true));
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

/// Follow-up advice for a stack jjpr cannot merge because it belongs to GitHub.
/// `None` when native-stack membership is not why it stopped.
///
/// Shared by the `merge` and `watch` summaries. Both otherwise end on a "try
/// again" line, and neither is true here: re-running resolves nothing, so the
/// only useful thing to say is which command does.
/// The follow-up line for a run that stopped at a blocked segment.
///
/// `merge` and `watch` each have their own end-of-run summary, and they used to
/// choose this line independently. That is how `watch` shipped telling users to
/// re-run `watch` for a native stack, which never clears it, while `merge` said
/// the right thing. Deciding it in one place makes that divergence structurally
/// impossible; the printers only print.
///
/// `from_watch` selects the two genuine differences: watch names itself in the
/// fallback, and has no "run watch to wait" branch because it *is* the wait.
fn blocked_follow_up(reasons: &[merge::plan::BlockReason], from_watch: bool) -> String {
    if reasons.iter().any(|r| matches!(r, merge::plan::BlockReason::NoPr)) {
        return "\nRun `jjpr submit` to create PRs, then re-run `jjpr watch`.".to_string();
    }
    if let Some(advice) = native_stack_advice(reasons) {
        return advice;
    }
    if !from_watch && reasons.iter().all(|r| r.is_transient()) {
        return "\nRun `jjpr watch` to wait and auto-continue.".to_string();
    }
    if from_watch {
        // LocalSyncFailed / ForgeReconcileFailed don't normally reach here
        // because watch keeps iterating through them. Anything else here is
        // fatal and rerunning watch is the right action.
        "\nRun `jjpr watch` again once the issue is resolved.".to_string()
    } else {
        "\nRun `jjpr merge` again once the issue is resolved.".to_string()
    }
}

fn native_stack_advice(reasons: &[merge::plan::BlockReason]) -> Option<String> {
    reasons.iter().find_map(|r| match r {
        merge::plan::BlockReason::NativeStack { stack_number, .. } => Some(format!(
            "\nThis stack is registered as a GitHub native stack, which jjpr cannot merge.\n\
             Merge it with `gh stack merge`, or dissolve the native stack with\n\
             `gh stack unstack {stack_number}` to hand merging back to jjpr."
        )),
        _ => None,
    })
}

fn print_merge_summary(result: &merge::execute::MergeResult) {
    if result.merged.is_empty() && result.skipped_merged.is_empty() && result.blocked_at.is_none() {
        println!("\nNo PRs to merge in this stack.");
    } else if let Some(ref blocked) = result.blocked_at {
        println!("{}", blocked_follow_up(&blocked.reasons, false));
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
    use jjpr::forge::types::{PrMergeability, ReviewSummary};

    // status printed "✓ mergeable" for PRs that `jjpr merge` refuses outright,
    // so the two commands disagreed about the same PR. Verified against a live
    // stack before this note existed.
    #[test]
    fn status_flags_a_natively_stacked_pr_as_unmergeable_by_jjpr() {
        let mut pr = pr_with("me", false, None);
        pr.stack = Some(jjpr::forge::types::PrStackRef {
            number: 223,
            id: 1,
            position: 2,
            size: 3,
            base: None,
        });
        let note = StatusRender::native_stack_note(&pr, true).expect("should flag it");
        assert!(note.contains("#223"), "names the stack: {note}");
        assert!(note.contains("2 of 3"), "says where in the stack: {note}");
        assert!(note.contains("gh stack merge 15138"), "gives the way to land it: {note}");
        // Merging a stacked PR lands everything below it. The merge block reason
        // spells that out; status must not be vaguer about the same command.
        assert!(note.contains("lands it and the 1 below"), "says what lands: {note}");
    }

    // At the bottom there is nothing below, so the note must not imply otherwise
    // — the same overstatement already fixed in the merge block reason.
    #[test]
    fn status_note_does_not_overstate_at_the_bottom_of_a_stack() {
        let mut pr = pr_with("me", false, None);
        pr.stack = Some(jjpr::forge::types::PrStackRef {
            number: 223, id: 1, position: 1, size: 2, base: None,
        });
        let note = StatusRender::native_stack_note(&pr, true).expect("should flag it");
        assert!(note.contains("lands it"), "{note}");
        assert!(!note.contains("below"), "nothing is below the bottom PR: {note}");
    }

    // PrStackRef defaults every field so a shrinking payload still parses. The
    // note must degrade to something true rather than print "#0 (0 of 0)".
    #[test]
    fn status_note_degrades_when_the_stack_payload_is_partial() {
        let mut pr = pr_with("me", false, None);
        pr.stack = Some(jjpr::forge::types::PrStackRef {
            number: 0, id: 0, position: 0, size: 0, base: None,
        });
        let note = StatusRender::native_stack_note(&pr, true).expect("membership still holds");
        assert!(!note.contains("#0"), "must not invent a stack number: {note}");
        assert!(!note.contains("0 of 0"), "must not print a bogus position: {note}");
        assert!(note.contains("a native stack"), "still states the fact: {note}");
        assert!(note.contains("gh stack merge"), "still gives the command: {note}");
    }

    // A foreign segment already says jjpr won't merge it; repeating why is noise.
    #[test]
    fn status_does_not_repeat_the_native_stack_note_for_foreign_prs() {
        let mut pr = pr_with("someone-else", false, None);
        pr.stack = Some(jjpr::forge::types::PrStackRef {
            number: 223, id: 1, position: 1, size: 2, base: None,
        });
        assert!(StatusRender::native_stack_note(&pr, false).is_none());
    }

    // The common case must stay silent.
    #[test]
    fn status_says_nothing_about_stacks_for_an_ordinary_pr() {
        let pr = pr_with("me", false, None);
        assert!(StatusRender::native_stack_note(&pr, true).is_none());
    }

    // The regression that started this: watch shipped telling users to re-run
    // watch for a native stack, which never clears it, while merge said the
    // right thing. Both now decide via `blocked_follow_up`, so assert BOTH
    // modes — testing only the helper let the wiring drift once already, and a
    // mutation test confirmed removing watch's call site broke no test.
    #[test]
    fn both_summaries_give_native_stack_advice_not_a_retry() {
        let reasons = vec![merge::plan::BlockReason::NativeStack {
            pr_number: 298,
            stack_number: 301,
            position: 1,
            size: 3,
        }];
        for from_watch in [false, true] {
            let line = blocked_follow_up(&reasons, from_watch);
            assert!(line.contains("gh stack unstack 301"), "from_watch={from_watch}: {line}");
            assert!(
                !line.contains("again once the issue is resolved"),
                "from_watch={from_watch} must not suggest re-running: {line}"
            );
        }
    }

    // The branches that legitimately differ between the two commands.
    #[test]
    fn blocked_follow_up_differs_only_where_it_should() {
        let transient = vec![merge::plan::BlockReason::ChecksPending];
        assert!(blocked_follow_up(&transient, false).contains("watch` to wait"));
        // watch IS the wait, so it has no such branch and falls through.
        assert!(blocked_follow_up(&transient, true).contains("watch` again once"));

        let hard = vec![merge::plan::BlockReason::ChangesRequested];
        assert!(blocked_follow_up(&hard, false).contains("merge` again once"));
        assert!(blocked_follow_up(&hard, true).contains("watch` again once"));

        // NoPr points at submit from either command.
        let nopr = vec![merge::plan::BlockReason::NoPr];
        for w in [false, true] {
            assert!(blocked_follow_up(&nopr, w).contains("jjpr submit"), "w={w}");
        }
    }

    #[test]
    fn native_stack_advice_replaces_the_useless_try_again_line() {
        let reasons = vec![merge::plan::BlockReason::NativeStack {
            pr_number: 298,
            stack_number: 301,
            position: 1,
            size: 3,
        }];
        let advice = native_stack_advice(&reasons).expect("should recognise the block");
        assert!(advice.contains("gh stack merge"), "{advice}");
        assert!(advice.contains("gh stack unstack 301"), "{advice}");
        assert!(
            !advice.contains("again once the issue is resolved"),
            "must not suggest re-running: {advice}"
        );
    }

    // Every other block keeps its existing advice.
    #[test]
    fn native_stack_advice_is_none_for_other_blocks() {
        assert!(native_stack_advice(&[merge::plan::BlockReason::ChecksPending]).is_none());
        assert!(native_stack_advice(&[]).is_none());
    }

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

    // --- author-agnostic status rendering ---

    fn named_bookmark(name: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: "c".to_string(),
            change_id: "z".to_string(),
            has_remote: true,
            is_synced: true,
        }
    }

    fn pr_with(author: &str, draft: bool, merged_at: Option<&str>) -> PullRequest {
        use jjpr::forge::types::PullRequestRef;
        let empty_ref = || PullRequestRef {
            ref_name: String::new(),
            label: String::new(),
            sha: String::new(),
        };
        PullRequest {
            number: 15138,
            html_url: "https://x/pull/15138".to_string(),
            title: "t".to_string(),
            body: None,
            base: empty_ref(),
            head: empty_ref(),
            draft,
            node_id: String::new(),
            merged_at: merged_at.map(str::to_string),
            requested_reviewers: vec![],
            author: author.to_string(),
            stack: None,
        }
    }

    /// A forge whose batch path can be told to answer fully, partially, or not
    /// at all, and which records every per-PR call so a test can prove the
    /// batch actually avoided them.
    struct BatchStub {
        /// PR numbers the batch answers. `None` means "no batch path at all",
        /// which is what a GraphQL failure or a non-GitHub forge looks like.
        batched: Option<Vec<u64>>,
        per_pr_calls: std::sync::Mutex<Vec<u64>>,
    }

    impl BatchStub {
        fn new(batched: Option<Vec<u64>>) -> Self {
            Self {
                batched,
                per_pr_calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<u64> {
            let mut v = self.per_pr_calls.lock().unwrap().clone();
            v.sort_unstable();
            v
        }
    }

    impl Forge for BatchStub {
        fn batch_pr_status(
            &self,
            _o: &str,
            _r: &str,
            prs: &[(u64, String)],
        ) -> Option<HashMap<u64, jjpr::forge::PrStatusBundle>> {
            let answerable = self.batched.as_ref()?;
            Some(
                prs.iter()
                    .filter(|(n, _)| answerable.contains(n))
                    .map(|(n, _)| {
                        (
                            *n,
                            jjpr::forge::PrStatusBundle {
                                mergeability: Some(PrMergeability {
                                    mergeable: Some(true),
                                    mergeable_state: "clean".to_string(),
                                }),
                                checks: Some(ChecksStatus::Pass),
                                reviews: Some(ReviewSummary {
                                    approved_count: 7,
                                    changes_requested: false,
                                }),
                            },
                        )
                    })
                    .collect(),
            )
        }
        fn get_pr_mergeability(&self, _o: &str, _r: &str, n: u64) -> Result<PrMergeability> {
            self.per_pr_calls.lock().unwrap().push(n);
            Ok(PrMergeability {
                mergeable: Some(false),
                mergeable_state: "dirty".to_string(),
            })
        }
        fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> {
            Ok(ChecksStatus::Fail)
        }
        fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> {
            Ok(ReviewSummary {
                approved_count: 0,
                changes_requested: true,
            })
        }
        fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
            unimplemented!()
        }
        fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _rev: &[String]) -> Result<()> {
            unimplemented!()
        }
        fn list_comments(&self, _o: &str, _r: &str, _n: u64) -> Result<Vec<jjpr::forge::types::IssueComment>> {
            unimplemented!()
        }
        fn create_comment(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<jjpr::forge::types::IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _o: &str, _r: &str, _c: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> {
            unimplemented!()
        }
        fn get_authenticated_user(&self) -> Result<String> {
            unimplemented!()
        }
        fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> {
            unimplemented!()
        }
        fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> {
            unimplemented!()
        }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<jjpr::forge::types::PrState> {
            unimplemented!()
        }
    }

    fn numbered_pr(number: u64) -> PullRequest {
        let mut pr = pr_with("me", false, None);
        pr.number = number;
        pr
    }

    fn targets(numbers: &[u64]) -> Vec<(String, PullRequest)> {
        numbers
            .iter()
            .map(|n| (format!("bm-{n}"), numbered_pr(*n)))
            .collect()
    }

    fn repo() -> RepoInfo {
        RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        }
    }

    #[test]
    fn batch_answers_everything_and_no_per_pr_calls_are_made() {
        let stub = BatchStub::new(Some(vec![1, 2, 3]));
        let out = fetch_all_segment_status(&stub, &repo(), &targets(&[1, 2, 3]));
        assert_eq!(out.len(), 3);
        assert!(stub.calls().is_empty(), "batch should have avoided per-PR calls");
        // The batched values, not the per-PR stub's, must be what lands.
        assert_eq!(out["bm-1"].reviews.as_ref().unwrap().approved_count, 7);
        assert_eq!(out["bm-1"].checks, Some(ChecksStatus::Pass));
    }

    #[test]
    fn no_batch_path_falls_back_to_every_pr() {
        // What a GraphQL failure, or GitLab/Forgejo, looks like.
        let stub = BatchStub::new(None);
        let out = fetch_all_segment_status(&stub, &repo(), &targets(&[1, 2, 3]));
        assert_eq!(out.len(), 3, "fallback must still cover every PR");
        assert_eq!(stub.calls(), vec![1, 2, 3]);
        // The per-PR values must land, proving the fallback data is used.
        assert_eq!(out["bm-2"].mergeability.as_ref().unwrap().mergeable, Some(false));
        assert_eq!(out["bm-2"].checks, Some(ChecksStatus::Fail));
    }

    #[test]
    fn a_partial_batch_only_fetches_the_gaps() {
        // A backend that can answer some PRs but not others must not lose the
        // rest, and must not re-fetch what it already has.
        let stub = BatchStub::new(Some(vec![1, 3]));
        let out = fetch_all_segment_status(&stub, &repo(), &targets(&[1, 2, 3]));
        assert_eq!(out.len(), 3, "batched and fetched PRs must both appear");
        assert_eq!(stub.calls(), vec![2], "only the gap should be fetched");
        assert_eq!(out["bm-1"].checks, Some(ChecksStatus::Pass), "from batch");
        assert_eq!(out["bm-2"].checks, Some(ChecksStatus::Fail), "from fallback");
        assert_eq!(out["bm-3"].checks, Some(ChecksStatus::Pass), "from batch");
    }

    #[test]
    fn no_targets_makes_no_calls() {
        let stub = BatchStub::new(Some(vec![1]));
        let out = fetch_all_segment_status(&stub, &repo(), &[]);
        assert!(out.is_empty());
        assert!(stub.calls().is_empty());
    }

    #[test]
    fn results_are_keyed_by_bookmark_not_pr_number() {
        // The render looks status up by bookmark name; mapping the wrong PR to a
        // bookmark would mislabel a whole segment.
        let stub = BatchStub::new(Some(vec![10, 20]));
        let out = fetch_all_segment_status(&stub, &repo(), &targets(&[10, 20]));
        assert!(out.contains_key("bm-10") && out.contains_key("bm-20"));
    }

    #[test]
    fn checks_ref_prefers_sha_and_falls_back_to_branch() {
        use jjpr::forge::types::PullRequestRef;
        let mut pr = numbered_pr(1);
        pr.head = PullRequestRef {
            ref_name: "my-branch".to_string(),
            label: String::new(),
            sha: "abc123".to_string(),
        };
        assert_eq!(pr.checks_ref(), "abc123");
        pr.head.sha = String::new();
        assert_eq!(pr.checks_ref(), "my-branch");
    }

    fn seg(names: &[&str]) -> BookmarkSegment {
        BookmarkSegment {
            bookmarks: names.iter().map(|n| named_bookmark(n)).collect(),
            changes: vec![],
            merge_source_names: vec![],
        }
    }

    fn bm(name: &str, has_remote: bool) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: "c".to_string(),
            change_id: "z".to_string(),
            has_remote,
            is_synced: has_remote,
        }
    }

    fn segment_of(bookmarks: Vec<Bookmark>) -> BookmarkSegment {
        BookmarkSegment { bookmarks, changes: vec![], merge_source_names: vec![] }
    }

    fn status_all_pass() -> SegmentDisplayStatus {
        SegmentDisplayStatus {
            mergeability: Some(PrMergeability { mergeable: Some(true), mergeable_state: String::new() }),
            checks: Some(ChecksStatus::Pass),
            reviews: Some(ReviewSummary { approved_count: 1, changes_requested: false }),
        }
    }

    #[test]
    fn pr_label_covers_ownership_and_state() {
        // Yours: no author attribution.
        assert_eq!(pr_label(&SegmentPr::Open(&pr_with("me", false, None)), true), ", PR open");
        assert_eq!(pr_label(&SegmentPr::Open(&pr_with("me", true, None)), true), ", PR draft");
        assert_eq!(
            pr_label(&SegmentPr::Merged(&pr_with("me", false, Some("2026-04-20T00:00:00Z"))), true),
            ", PR merged"
        );
        // Someone else's: attributed.
        assert_eq!(
            pr_label(&SegmentPr::Open(&pr_with("dana", false, None)), false),
            ", PR open by @dana"
        );
        assert_eq!(
            pr_label(&SegmentPr::Merged(&pr_with("jasonziaja", false, Some("2026-04-20"))), false),
            ", PR merged by @jasonziaja"
        );
        // Foreign PR with an unknown author still reads sensibly.
        assert_eq!(
            pr_label(&SegmentPr::Open(&pr_with("", false, None)), false),
            ", PR open by someone else"
        );
        assert_eq!(pr_label(&SegmentPr::None, false), "");
    }

    #[test]
    fn merged_on_suffix_takes_the_date() {
        assert_eq!(merged_on_suffix(&pr_with("x", false, Some("2026-04-20T14:43:59Z"))), " on 2026-04-20");
        assert_eq!(merged_on_suffix(&pr_with("x", false, None)), "");
        assert_eq!(merged_on_suffix(&pr_with("x", false, Some("short"))), "");
    }

    #[test]
    fn mergeability_line_variants() {
        let with = |m| SegmentDisplayStatus {
            mergeability: Some(PrMergeability { mergeable: m, mergeable_state: String::new() }),
            checks: None,
            reviews: None,
        };
        assert_eq!(format_mergeability_line(&with(Some(true))), "    \u{2713} mergeable");
        assert_eq!(format_mergeability_line(&with(Some(false))), "    \u{2717} conflicts");
        assert_eq!(format_mergeability_line(&with(None)), "");
    }

    fn render_with<'a>(
        pr_map: &'a HashMap<String, PullRequest>,
        merged_map: &'a HashMap<String, PullRequest>,
        my_names: &'a HashSet<String>,
        status_map: &'a HashMap<String, SegmentDisplayStatus>,
    ) -> StatusRender<'a> {
        StatusRender {
            pr_map,
            merged_map,
            status_map,
            my_names,
            identity: None,
            has_forge: true,
            trunk_dismisses_stale: None,
            below_pr: HashMap::new(),
            forge_kind: None,
        }
    }

    #[test]
    fn header_slot_reflects_ownership_and_state() {
        let (pr_map, merged_map, status_map) = (HashMap::new(), HashMap::new(), HashMap::new());
        let my: HashSet<String> = HashSet::new();
        let r = render_with(&pr_map, &merged_map, &my, &status_map);
        let bms = [named_bookmark("b")];

        // Yours → the push/sync state.
        assert_eq!(
            r.header_slot(&SegmentPr::None, true, &bms).as_deref(),
            Some("push up to date")
        );
        // Someone else's still-open PR → the leave-alone note.
        assert_eq!(
            r.header_slot(&SegmentPr::Open(&pr_with("dana", false, None)), false, &bms).as_deref(),
            Some("jjpr won't submit or merge it")
        );
        // A merged segment (yours or not) has no push/sync slot.
        assert_eq!(
            r.header_slot(&SegmentPr::Merged(&pr_with("x", false, Some("2026-04-20"))), false, &bms),
            None
        );
        assert_eq!(
            r.header_slot(&SegmentPr::Merged(&pr_with("x", false, Some("2026-04-20"))), true, &bms),
            None
        );
        assert_eq!(r.header_slot(&SegmentPr::None, false, &bms), None);
    }

    #[test]
    fn resolve_pr_prefers_open_then_merged() {
        let mut pr_map = HashMap::new();
        pr_map.insert("open-one".to_string(), pr_with("a", false, None));
        let mut merged_map = HashMap::new();
        merged_map.insert("merged-one".to_string(), pr_with("b", false, Some("2026-04-20")));
        let (my, status_map) = (HashSet::new(), HashMap::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        assert!(matches!(r.resolve_pr("open-one"), SegmentPr::Open(_)));
        assert!(matches!(r.resolve_pr("merged-one"), SegmentPr::Merged(_)));
        assert!(matches!(r.resolve_pr("nope"), SegmentPr::None));
    }

    #[test]
    fn ownership_and_all_foreign_detection() {
        let (pr_map, merged_map, status_map) = (HashMap::new(), HashMap::new(), HashMap::new());
        let my: HashSet<String> = ["mine-feat".to_string()].into_iter().collect();
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        assert!(r.segment_is_mine(&seg(&["mine-feat"])));
        assert!(!r.segment_is_mine(&seg(&["coworker-feat"])));

        let all_foreign = BranchStack { segments: vec![seg(&["coworker-feat"])], base_branch: None };
        let has_mine = BranchStack {
            segments: vec![seg(&["mine-feat"]), seg(&["coworker-feat"])],
            base_branch: None,
        };
        let empty = BranchStack { segments: vec![], base_branch: None };
        assert!(r.stack_is_all_foreign(&all_foreign));
        assert!(!r.stack_is_all_foreign(&has_mine));
        assert!(!r.stack_is_all_foreign(&empty));
    }

    #[test]
    fn renders_mine_open_pr_with_url_and_full_status() {
        let mut pr_map = HashMap::new();
        pr_map.insert("mine".to_string(), pr_with("me", false, None));
        let mut status_map = HashMap::new();
        status_map.insert("mine".to_string(), status_all_pass());
        let (merged_map, my) = (HashMap::new(), ["mine".to_string()].into_iter().collect());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let lines = r.render_segment(&seg(&["mine"]));
        assert!(lines[0].contains(", PR open,"), "{lines:?}");
        assert!(!lines[0].contains("by @"), "yours is not attributed: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("pull/15138")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("CI passing")), "{lines:?}");
    }

    #[test]
    fn dismiss_stale_note_conditions() {
        let mut pr_map = HashMap::new();
        pr_map.insert("upper".to_string(), pr_with("me", false, None));
        let merged_map = HashMap::new();
        let my: HashSet<String> = ["upper".to_string()].into_iter().collect();

        let unreviewed = |approved: u32| SegmentDisplayStatus {
            mergeability: None,
            checks: None,
            reviews: Some(ReviewSummary { approved_count: approved, changes_requested: false }),
        };
        let mut status_map = HashMap::new();
        status_map.insert("upper".to_string(), status_all_pass()); // approved_count 1
        status_map.insert("multi".to_string(), unreviewed(2));
        status_map.insert("unapproved".to_string(), unreviewed(0));

        let mut below_pr = HashMap::new();
        below_pr.insert("upper".to_string(), 123);
        below_pr.insert("multi".to_string(), 200);
        below_pr.insert("unapproved".to_string(), 99);

        let mk = |dismiss, kind| StatusRender {
            pr_map: &pr_map,
            merged_map: &merged_map,
            status_map: &status_map,
            my_names: &my,
            identity: None,
            has_forge: true,
            trunk_dismisses_stale: dismiss,
            below_pr: below_pr.clone(),
            forge_kind: kind,
        };
        let note = |r: StatusRender, name: &str, mine: bool| r.dismiss_stale_note(name, mine);

        // Approved PR stacked atop #123 with a dismiss-stale trunk → warn.
        assert_eq!(
            note(mk(Some(true), Some(ForgeKind::GitHub)), "upper", true).as_deref(),
            Some("    \u{26a0} a squash-landing of #123 would dismiss 1 approval"),
        );
        // GitLab renders the reference as !123, and plural approvals agree in number.
        assert_eq!(
            note(mk(Some(true), Some(ForgeKind::GitLab)), "multi", true).as_deref(),
            Some("    \u{26a0} a squash-landing of !200 would dismiss 2 approvals"),
        );
        // Trunk does not dismiss (or is undetermined) → no note.
        assert_eq!(note(mk(Some(false), Some(ForgeKind::GitHub)), "upper", true), None);
        assert_eq!(note(mk(None, Some(ForgeKind::GitHub)), "upper", true), None);
        // Not yours → silent (the note is about *your* approvals being dropped).
        assert_eq!(note(mk(Some(true), Some(ForgeKind::GitHub)), "upper", false), None);
        // Nothing stacked below → no landing would rebase it.
        assert_eq!(note(mk(Some(true), Some(ForgeKind::GitHub)), "upper-no-below", true), None);
        // Stacked, but this PR has no approvals to lose.
        assert_eq!(note(mk(Some(true), Some(ForgeKind::GitHub)), "unapproved", true), None);
    }

    #[test]
    fn render_segment_wires_in_dismiss_stale_note() {
        let mut pr_map = HashMap::new();
        pr_map.insert("upper".to_string(), pr_with("me", false, None));
        let mut status_map = HashMap::new();
        status_map.insert("upper".to_string(), status_all_pass());
        let (merged_map, my): (HashMap<String, PullRequest>, HashSet<String>) =
            (HashMap::new(), ["upper".to_string()].into_iter().collect());
        let mut below_pr = HashMap::new();
        below_pr.insert("upper".to_string(), 123);

        let r = StatusRender {
            pr_map: &pr_map,
            merged_map: &merged_map,
            status_map: &status_map,
            my_names: &my,
            identity: None,
            has_forge: true,
            trunk_dismisses_stale: Some(true),
            below_pr,
            forge_kind: Some(ForgeKind::GitHub),
        };
        let lines = r.render_segment(&seg(&["upper"]));
        assert!(
            lines.iter().any(|l| l.contains("squash-landing of #123 would dismiss 1 approval")),
            "note missing: {lines:?}"
        );
        // The ordinary status line still renders alongside the note.
        assert!(lines.iter().any(|l| l.contains("CI passing")), "{lines:?}");
    }

    // Mirrors the dismiss-stale wiring test above, and for the same reason:
    // testing `native_stack_note` alone proves nothing about whether it reaches
    // the output. Deleting the call site left every other test green.
    #[test]
    fn render_segment_wires_in_the_native_stack_note() {
        let mut stacked = pr_with("me", false, None);
        stacked.stack = Some(jjpr::forge::types::PrStackRef {
            number: 348,
            id: 1,
            position: 2,
            size: 3,
            base: None,
        });
        let mut pr_map = HashMap::new();
        pr_map.insert("mine".to_string(), stacked);
        let mut status_map = HashMap::new();
        status_map.insert("mine".to_string(), status_all_pass());
        let (merged_map, my): (HashMap<String, PullRequest>, HashSet<String>) =
            (HashMap::new(), ["mine".to_string()].into_iter().collect());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let lines = r.render_segment(&seg(&["mine"]));
        assert!(
            lines.iter().any(|l| l.contains("in native stack #348 (2 of 3)")),
            "note missing from rendered output: {lines:?}"
        );
        // And it does not displace the ordinary status line.
        assert!(lines.iter().any(|l| l.contains("CI passing")), "{lines:?}");
    }

    // The common case must not gain a line.
    #[test]
    fn render_segment_stays_quiet_for_an_unstacked_pr() {
        let mut pr_map = HashMap::new();
        pr_map.insert("mine".to_string(), pr_with("me", false, None));
        let mut status_map = HashMap::new();
        status_map.insert("mine".to_string(), status_all_pass());
        let (merged_map, my): (HashMap<String, PullRequest>, HashSet<String>) =
            (HashMap::new(), ["mine".to_string()].into_iter().collect());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let lines = r.render_segment(&seg(&["mine"]));
        assert!(
            !lines.iter().any(|l| l.contains("native stack")),
            "should say nothing about stacks: {lines:?}"
        );
    }

    #[test]
    fn draft_pr_suppresses_dismiss_stale_note() {
        // A draft hides its review detail, so it must hide the review-loss note
        // too — even with an approval and a dismiss-stale trunk below it.
        let mut pr_map = HashMap::new();
        pr_map.insert("upper".to_string(), pr_with("me", true, None)); // draft
        let mut status_map = HashMap::new();
        status_map.insert("upper".to_string(), status_all_pass());
        let (merged_map, my): (HashMap<String, PullRequest>, HashSet<String>) =
            (HashMap::new(), ["upper".to_string()].into_iter().collect());
        let mut below_pr = HashMap::new();
        below_pr.insert("upper".to_string(), 123);

        let r = StatusRender {
            pr_map: &pr_map,
            merged_map: &merged_map,
            status_map: &status_map,
            my_names: &my,
            identity: None,
            has_forge: true,
            trunk_dismisses_stale: Some(true),
            below_pr,
            forge_kind: Some(ForgeKind::GitHub),
        };
        let lines = r.render_segment(&seg(&["upper"]));
        assert!(
            !lines.iter().any(|l| l.contains("dismiss")),
            "draft must not show the dismiss note: {lines:?}"
        );
    }

    #[test]
    fn renders_mine_without_pr_as_submit_hint() {
        let (pr_map, merged_map, status_map) = (HashMap::new(), HashMap::new(), HashMap::new());
        let my: HashSet<String> = ["mine".to_string()].into_iter().collect();
        let r = render_with(&pr_map, &merged_map, &my, &status_map);
        let lines = r.render_segment(&seg(&["mine"]));
        assert!(lines.iter().any(|l| l.contains("no PR yet")), "{lines:?}");
    }

    #[test]
    fn renders_foreign_open_pr_attributed_with_mergeability_only() {
        let mut pr_map = HashMap::new();
        pr_map.insert("dana-feat".to_string(), pr_with("dana", false, None));
        let mut status_map = HashMap::new();
        // CI fails, but a foreign segment should show mergeability only.
        status_map.insert(
            "dana-feat".to_string(),
            SegmentDisplayStatus {
                mergeability: Some(PrMergeability { mergeable: Some(true), mergeable_state: String::new() }),
                checks: Some(ChecksStatus::Fail),
                reviews: Some(ReviewSummary { approved_count: 0, changes_requested: false }),
            },
        );
        let (merged_map, my) = (HashMap::new(), HashSet::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let lines = r.render_segment(&seg(&["dana-feat"]));
        assert!(lines[0].contains("PR open by @dana"), "{lines:?}");
        assert!(lines[0].contains("jjpr won't submit or merge it"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("mergeable")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("CI")), "foreign shows mergeability only: {lines:?}");
    }

    #[test]
    fn renders_foreign_merged_stale_with_cleanup_hint() {
        let mut merged_map = HashMap::new();
        merged_map.insert("gone".to_string(), pr_with("dana", false, Some("2026-04-20T00:00:00Z")));
        let (pr_map, status_map, my) = (HashMap::new(), HashMap::new(), HashSet::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        // Remote branch deleted (has_remote = false) → stale, with cleanup.
        let lines = r.render_segment(&segment_of(vec![bm("gone", false)]));
        assert!(lines[0].contains("PR merged by @dana"), "{lines:?}");
        assert!(!lines[0].contains("won't submit"), "merged carries no slot: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("merged on 2026-04-20")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("local bookmark is stale")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("jj bookmark forget gone")), "{lines:?}");
    }

    #[test]
    fn renders_foreign_merged_with_live_remote_omits_cleanup() {
        let mut merged_map = HashMap::new();
        merged_map.insert("kept".to_string(), pr_with("dana", false, Some("2026-04-20")));
        let (pr_map, status_map, my) = (HashMap::new(), HashMap::new(), HashSet::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let lines = r.render_segment(&segment_of(vec![bm("kept", true)]));
        assert!(lines.iter().any(|l| l.contains("merged on 2026-04-20")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("stale")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("forget")), "{lines:?}");
    }

    #[test]
    fn recognition_screen_for_an_all_foreign_stack() {
        let mut merged_map = HashMap::new();
        merged_map.insert("cycle".to_string(), pr_with("dana", false, Some("2026-04-20")));
        let (pr_map, status_map, my) = (HashMap::new(), HashMap::new(), HashSet::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let stack = BranchStack {
            segments: vec![segment_of(vec![bm("cycle", false)])],
            base_branch: Some("coworker-base".to_string()),
        };
        let lines = r.render_foreign_only(&stack);
        assert_eq!(lines[0], "On cycle \u{2014} someone else's merged branch:");
        // The foreign base must not be dropped in the recognition path.
        assert!(lines.iter().any(|l| l == "  (based on coworker-base)"), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "Nothing of yours to submit here.");
    }

    #[test]
    fn render_base_attributes_an_open_foreign_base() {
        let mut pr_map = HashMap::new();
        pr_map.insert("platform".to_string(), pr_with("dana", false, None));
        let (merged_map, my, status_map) = (HashMap::new(), HashSet::new(), HashMap::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let lines = r.render_base("platform");
        assert_eq!(lines[0], "  (based on platform \u{2014} PR open by @dana)");
        assert!(lines[1].contains("pull/15138"), "{lines:?}");
    }

    #[test]
    fn render_base_attributes_a_merged_foreign_base() {
        let mut merged_map = HashMap::new();
        merged_map.insert("platform".to_string(), pr_with("dana", false, Some("2026-04-20")));
        let (pr_map, my, status_map) = (HashMap::new(), HashSet::new(), HashMap::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        let lines = r.render_base("platform");
        assert_eq!(lines[0], "  (based on platform \u{2014} PR merged by @dana)");
        assert!(lines[1].contains("pull/15138"), "{lines:?}");
    }

    #[test]
    fn render_base_stays_bare_without_a_pr() {
        let (pr_map, merged_map, my, status_map) =
            (HashMap::new(), HashMap::new(), HashSet::new(), HashMap::new());
        let r = render_with(&pr_map, &merged_map, &my, &status_map);
        assert_eq!(r.render_base("platform"), vec!["  (based on platform)".to_string()]);
    }

    #[test]
    fn login_supplement_reclassifies_a_foreign_pr_you_authored() {
        // Foreign by EMAIL (bookmark not in my_names), but the PR was authored
        // by your forge login — your own work from another machine's email.
        let mut merged_map = HashMap::new();
        merged_map.insert("feat".to_string(), pr_with("michaeldhopkins", false, Some("2026-03-12")));
        let (pr_map, my, status_map) = (HashMap::new(), HashSet::new(), HashMap::new());
        let mut identity = Identity::default();
        identity.add_login("michaeldhopkins");

        let r = StatusRender {
            pr_map: &pr_map,
            merged_map: &merged_map,
            status_map: &status_map,
            my_names: &my,
            identity: Some(&identity),
            has_forge: true,
            trunk_dismisses_stale: None,
            below_pr: HashMap::new(),
            forge_kind: None,
        };
        let seg = segment_of(vec![bm("feat", false)]);
        assert!(r.segment_is_yours(&seg), "your own PR (by login) must count as yours");
        let stack = BranchStack { segments: vec![seg], base_branch: None };
        assert!(
            !r.stack_is_all_foreign(&stack),
            "an all-foreign-by-email stack that is yours by login must not trigger recognition"
        );

        // Without the login supplement (identity None) it stays foreign.
        let r2 = render_with(&pr_map, &merged_map, &my, &status_map);
        assert!(!r2.segment_is_yours(&segment_of(vec![bm("feat", false)])));
    }

    #[test]
    fn multi_bookmark_segment_is_mine_if_any_bookmark_is_mine() {
        let (pr_map, merged_map, status_map) = (HashMap::new(), HashMap::new(), HashMap::new());
        let my: HashSet<String> = ["my-feat".to_string()].into_iter().collect();
        let r = render_with(&pr_map, &merged_map, &my, &status_map);

        // A coworker bookmark listed FIRST, yours second — still yours, so it
        // must not trigger the "nothing of yours" recognition screen.
        let segment = segment_of(vec![bm("coworker-feat", true), bm("my-feat", true)]);
        assert!(r.segment_is_mine(&segment));
        let stack = BranchStack { segments: vec![segment], base_branch: None };
        assert!(!r.stack_is_all_foreign(&stack));
    }
}
