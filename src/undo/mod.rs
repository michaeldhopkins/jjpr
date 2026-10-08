//! `jjpr undo` and `jjpr redo`: take back the newest jjpr command whole, the
//! local repo and what it changed on the forge, or put it back again.
//!
//! `submit`, `merge` and `watch` record as they go ([`Recorder`], fed by
//! [`RecordingJj`] and [`RecordingForge`]). Undo checks everything first: that
//! nobody has acted since, and that every recorded change can be reversed
//! ([`plan`], which names each [`plan::Blocker`]). It changes nothing unless
//! it can take back the whole command. Then it carries the plan out
//! ([`execute`]), and if a step fails it puts back the steps it took
//! ([`rollback`]). The design and its measurements are in `notes/undo.md`.

pub mod execute;
pub mod explain;
pub mod failed;
pub mod journal;
#[cfg(test)]
mod model;
mod observe;
pub mod plan;
mod planner;
pub mod recorder;
pub mod recording_forge;
pub mod recording_jj;
pub mod repo;
pub mod report;
pub mod rollback;
mod show;
pub mod step_back;
#[cfg(test)]
mod world;

use std::io::Write;
use std::path::Path;

use anyhow::Result;

use crate::forge::Forge;

pub use journal::Journal;
pub use plan::Direction;
pub use recorder::{Meta, Recorder};
pub use recording_forge::RecordingForge;
pub use recording_jj::RecordingJj;
pub use repo::{JjRepo, UndoRepo};

use journal::{Entry, State};
use plan::{Blocker, Observed};
use report::Blocked;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub direction: Direction,
    pub force: bool,
    pub dry_run: bool,
}

impl Options {
    pub fn new(direction: Direction, force: bool, dry_run: bool) -> Self {
        Self {
            direction,
            force,
            dry_run,
        }
    }
}

/// Everything undo talks to, so tests can supply their own.
pub struct Context<'a> {
    pub journal: &'a Journal,
    pub repo: &'a dyn UndoRepo,
    /// Builds the forge client for the entry's remote, or says why it cannot.
    pub forge_for: &'a dyn Fn(&Entry) -> Result<Box<dyn Forge>>,
    pub watch_running: bool,
    pub now: u64,
}

pub fn run(cx: &Context, opts: Options, out: &mut dyn Write) -> Result<()> {
    let _lock = if opts.dry_run {
        None
    } else {
        Some(cx.journal.lock()?)
    };
    let loaded = cx.journal.load()?;
    if !loaded.unreadable.is_empty() {
        eprintln!(
            "  Warning: skipped undo journal entries that could not be read: {}",
            loaded.unreadable.join(", ")
        );
    }
    let target = match opts.direction {
        Direction::Undo => journal::undo_target(&loaded.entries),
        Direction::Redo => journal::redo_target(&loaded.entries),
    };
    let Some(target) = target else {
        writeln!(out, "{}", report::nothing(opts.direction))?;
        return Ok(());
    };
    let mut entry = target.clone();
    // A submit or watch recording right now would race the restore and the
    // pushes. The entry it is writing says so.
    let me = std::process::id();
    if let Some(pid) = loaded
        .entries
        .iter()
        .filter(|e| e.state == State::Running)
        .filter_map(|e| journal::pid_of(&e.id))
        .find(|&pid| pid != me && journal::alive(pid))
    {
        let text = report::blocked(&Blocked::Busy(pid), &entry, opts.direction, cx.now);
        anyhow::bail!("{text}");
    }
    if entry.state == State::Running && !journal::pid_of(&entry.id).is_some_and(journal::alive) {
        finish_abandoned(cx, &mut entry, opts)?;
        writeln!(out, "{}", report::abandoned(&entry, cx.now))?;
    }
    let say = |b: Blocked| report::blocked(&b, &entry, opts.direction, cx.now);
    if let Some(b) = check_ready(cx, &entry, opts)? {
        // jj dropped the operation, so this entry can never be acted on; an
        // undo cannot reach anything older either.
        if let (Blocked::OpGone(_), false) = (&b, opts.dry_run) {
            if opts.direction == Direction::Undo {
                cx.journal.prune_before(&entry.id)?;
            }
            cx.journal.remove(&entry.id)?;
        }
        anyhow::bail!("{}", say(b));
    }
    let refuse = |blockers: &[Blocker]| {
        explain::refused(blockers, &entry, opts.direction, opts.force, cx.now)
    };
    if let Some(number) = entry.merged() {
        let text = refuse(&[Blocker::Merged { number }]).unwrap_or_default();
        anyhow::bail!("{text}");
    }
    let mut blockers = check_local(cx, &entry, opts)?;
    // An entry that touched only the local repo (a step back over jj work)
    // needs no forge.
    let found = if entry.actions.is_empty() {
        Ok((None, Observed::default()))
    } else {
        (cx.forge_for)(&entry).and_then(|forge| {
            observe::observe(forge.as_ref(), &entry, opts).map(|o| (Some(forge), o))
        })
    };
    let (forge, observed) = match found {
        Ok(found) => found,
        // What the repo alone shows is reason enough; say that, not the forge's trouble.
        Err(e) if !blockers.is_empty() => {
            let text = refuse(&blockers).unwrap_or_default();
            anyhow::bail!("{text}\n(jjpr could not check the forge as well: {e:#})");
        }
        Err(e) => return Err(e),
    };
    let mut plan = plan::plan(&entry, opts.direction, &observed);
    blockers.append(&mut plan.blockers);
    plan.blockers = blockers;
    if opts.direction == Direction::Undo
        && step_back::try_it(cx, &entry, &mut plan.blockers, opts, out)?
    {
        return Ok(());
    }
    if opts.dry_run {
        return show::dry_run(out, &entry, &plan, opts, cx.now);
    }
    if let Some(text) = refuse(&plan.blockers) {
        anyhow::bail!("{text}");
    }
    writeln!(
        out,
        "{}",
        report::header(&entry, opts.direction, false, cx.now)
    )?;
    for b in &plan.blockers {
        if let Some(line) = explain::overridden(b, &entry) {
            writeln!(out, "{line}")?;
        }
    }
    let target = execute::Target {
        repo: cx.repo,
        forge: forge.as_deref(),
        journal: cx.journal,
    };
    let expected = cx.repo.view_fingerprint().ok();
    if let Err(stopped) = execute::run(&target, &mut entry, &plan, opts.direction, out) {
        let check = Check {
            forge: forge.as_deref(),
            before: &observed,
            view_before: expected,
        };
        anyhow::bail!("{}", after_failure(cx, &entry, opts, check, stopped));
    }
    show::kept(out, &plan, &entry, opts.direction)?;
    writeln!(out, "{}", report::done(&entry, opts.direction, cx.now))?;
    Ok(())
}

/// What a failed run is compared with afterwards.
struct Check<'a> {
    forge: Option<&'a dyn Forge>,
    before: &'a Observed,
    view_before: Option<String>,
}

/// What to say once a run failed at a step: how far putting it back got,
/// and, when it got all the way, whether the forge agrees.
fn after_failure(
    cx: &Context,
    entry: &Entry,
    opts: Options,
    check: Check,
    stopped: execute::Stopped,
) -> String {
    let cause = format!("{:#}", stopped.cause);
    if let Err(e) = &stopped.put_back {
        return failed::stopped_partway(entry, opts.direction, cx.now, &cause, &format!("{e:#}"));
    }
    let mut left = Vec::new();
    if cx.repo.view_fingerprint().ok() != check.view_before {
        left.push(rollback::Difference::Local);
    }
    if let Some(forge) = check.forge {
        match observe::observe(forge, entry, opts) {
            Ok(after) => left.extend(rollback::differences(check.before, &after)),
            Err(e) => eprintln!("  Warning: could not check the forge afterwards: {e:#}"),
        }
    }
    failed::stopped_and_put_back(entry, opts.direction, cx.now, &cause, &left)
}

/// The process that recorded `entry` died before finishing it (Ctrl-C, a
/// crash). What it recorded stands, and it saved where its own jj work
/// ended, so the record is completed from that.
fn finish_abandoned(cx: &Context, entry: &mut Entry, opts: Options) -> Result<()> {
    let end = entry
        .end_op
        .clone()
        .unwrap_or_else(|| entry.start_op.clone());
    entry.end_view = Some(cx.repo.view_fingerprint_at(&end)?);
    entry.end_op = Some(end);
    entry.state = State::Done;
    if !opts.dry_run {
        cx.journal.save(entry)?;
    }
    Ok(())
}

/// What stops undo before anything else is looked at: a watch running, an
/// entry still being recorded, or one jj can no longer restore.
fn check_ready(cx: &Context, entry: &Entry, opts: Options) -> Result<Option<Blocked>> {
    if cx.watch_running {
        return Ok(Some(Blocked::WatchRunning));
    }
    if entry.state == State::Running {
        return Ok(Some(Blocked::StillRunning));
    }
    let Some((_, restore_to)) = local_targets(entry) else {
        return Ok(Some(Blocked::NoEnd));
    };
    let restores = (opts.direction == Direction::Undo) != entry.local_undone;
    if restores && !cx.repo.op_exists(restore_to)? {
        return Ok(Some(Blocked::OpGone(restore_to.clone())));
    }
    Ok(None)
}

/// The fingerprint the repo must still have, and the operation the local
/// restore goes to.
fn local_targets(entry: &Entry) -> Option<(&String, &String)> {
    let (expected, restore_to) = if entry.local_undone {
        (entry.undone_view.as_ref(), entry.end_op.as_ref())
    } else {
        (entry.end_view.as_ref(), Some(&entry.start_op))
    };
    Some((expected?, restore_to?))
}

/// What jj shows that would be lost: work recorded while the command ran,
/// and any change to the repo since it (or the last undo of it) ended.
fn check_local(cx: &Context, entry: &Entry, opts: Options) -> Result<Vec<Blocker>> {
    let mut blockers = Vec::new();
    if opts.direction == Direction::Undo && !entry.local_undone && !entry.absorbed.is_empty() {
        blockers.push(Blocker::Absorbed(entry.absorbed.clone()));
    }
    let Some((expected, _)) = local_targets(entry) else {
        return Ok(blockers);
    };
    // A dry run changes nothing, so it leaves edits on disk unseen (and says so).
    if !opts.dry_run {
        cx.repo.snapshot()?;
    }
    if &cx.repo.view_fingerprint()? != expected {
        let last = entry.last_op.as_ref().or(entry.end_op.as_ref());
        let since = match last {
            Some(op) => cx
                .repo
                .ops_since(op, repo::OPS_SEARCHED)?
                .unwrap_or_default(),
            None => Vec::new(),
        };
        blockers.push(Blocker::RepoChanged { since });
    }
    Ok(blockers)
}

/// `jjpr undo` / `jjpr redo` from the command line.
pub fn command(opts: Options, list: bool) -> Result<()> {
    let root = crate::connect::find_repo_root()?;
    let journal = Journal::for_repo(&root)?;
    let now = now_secs();
    let mut out = std::io::stdout();
    if list {
        for line in report::list(&journal.load()?.entries, now) {
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }
    let repo = JjRepo::new(root.clone());
    let forge_for = |entry: &Entry| forge_for_entry(&root, entry, now);
    let cx = Context {
        journal: &journal,
        repo: &repo,
        forge_for: &forge_for,
        watch_running: crate::heartbeat::watch_running(&root),
        now,
    };
    run(&cx, opts, &mut out)
}

fn forge_for_entry(root: &Path, entry: &Entry, now: u64) -> Result<Box<dyn Forge>> {
    use crate::jj::Jj;
    let jj = crate::jj::JjRunner::new(root.to_path_buf())?;
    let remotes = jj.get_git_remotes()?;
    let say = |b: Blocked| report::blocked(&b, entry, Direction::Undo, now);
    if !remotes.iter().any(|r| r.name == entry.remote) {
        anyhow::bail!("{}", say(Blocked::NoRemote));
    }
    let cfg = crate::config::load_config_with_repo(Some(root))?;
    let resolved = crate::connect::resolve_forge(&remotes, &cfg, Some(&entry.remote))?;
    let same = resolved.kind == entry.forge
        && resolved.repo_info.owner == entry.owner
        && resolved.repo_info.repo == entry.repo;
    if !same {
        anyhow::bail!("{}", say(Blocked::OtherRepo));
    }
    Ok(resolved.forge)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Start recording a command for `jjpr undo`.
pub fn record(repo_root: &Path, meta: Meta) -> std::sync::Arc<Recorder> {
    let journal = Journal::for_repo(repo_root).unwrap_or_else(|_| {
        Journal::at(repo_root.join(".jj").join("repo").join("jjpr").join("undo"))
    });
    Recorder::start(
        journal,
        Box::new(JjRepo::new(repo_root.to_path_buf())),
        meta,
    )
}
