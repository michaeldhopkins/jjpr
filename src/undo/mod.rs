//! `jjpr undo` and `jjpr redo`: take back the newest jjpr command whole, the
//! local repo and what it changed on the forge, or put it back again.
//!
//! `submit`, `merge` and `watch` record as they go ([`Recorder`], fed by
//! [`RecordingJj`] and [`RecordingForge`]). Undo checks that nobody has acted
//! since, plans the reverse of each recorded change ([`plan`]), and carries it
//! out ([`execute`]). The design and its measurements are in `notes/undo.md`.

pub mod execute;
pub mod journal;
mod observe;
pub mod plan;
mod planner;
pub mod recorder;
pub mod recording_forge;
pub mod recording_jj;
pub mod repo;
pub mod report;

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
use plan::Kept;
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
        Direction::Undo => journal::undo_target(&loaded.entries, opts.force),
        Direction::Redo => journal::redo_target(&loaded.entries),
    };
    let Some(target) = target else {
        writeln!(out, "{}", report::nothing(opts.direction))?;
        return Ok(());
    };
    let mut entry = target.clone();
    let blocked =
        |b: Blocked| anyhow::anyhow!("{}", report::blocked(&b, &entry, opts.direction, cx.now));
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
        return Err(blocked(Blocked::Busy(pid)));
    }
    if entry.state == State::KeptOpen
        && let Some(older) = loaded
            .entries
            .iter()
            .find(|e| e.id < entry.id && e.state.undone())
    {
        return Err(blocked(Blocked::RedoFirst(report::name(older, cx.now))));
    }
    if entry.state == State::Running && !journal::pid_of(&entry.id).is_some_and(journal::alive) {
        finish_abandoned(cx, &mut entry, opts)?;
        writeln!(out, "{}", report::abandoned(&entry, cx.now))?;
    }
    let say = |b: Blocked| report::blocked(&b, &entry, opts.direction, cx.now);
    if let Some(b) = check_local(cx, &entry, opts)? {
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
    if let Some(number) = entry.merged() {
        let r = plan::Refusal::Merged { number };
        anyhow::bail!("{}", report::refusal(&r, &entry, opts.direction, cx.now));
    }
    let forge = (cx.forge_for)(&entry)?;
    let observed = observe::observe(forge.as_ref(), &entry, opts)?;
    let plan = plan::plan(&entry, opts.direction, &observed, opts.force)
        .map_err(|r| anyhow::anyhow!("{}", report::refusal(&r, &entry, opts.direction, cx.now)))?;
    print_plan(out, &entry, &plan, opts, cx.now)?;
    if opts.dry_run || (plan.steps.is_empty() && !plan.left.is_empty()) {
        return Ok(());
    }
    let target = execute::Target {
        repo: cx.repo,
        forge: forge.as_ref(),
        journal: cx.journal,
    };
    execute::run(&target, &mut entry, &plan, opts.direction, out)?;
    print_kept(out, &entry, &plan, opts.direction)?;
    writeln!(out, "{}", report::done(&entry, opts.direction, cx.now))?;
    Ok(())
}

fn print_plan(
    out: &mut dyn Write,
    entry: &Entry,
    plan: &plan::Plan,
    opts: Options,
    now: u64,
) -> Result<()> {
    let fk = entry.forge;
    if plan.steps.is_empty() && !plan.left.is_empty() {
        writeln!(
            out,
            "Nothing more to undo in {} without --force.",
            report::name(entry, now)
        )?;
        return print_kept(out, entry, plan, opts.direction);
    }
    writeln!(
        out,
        "{}",
        report::header(entry, opts.direction, opts.dry_run, now)
    )?;
    for c in &plan.overridden {
        writeln!(out, "{}", report::overridden(c, fk))?;
    }
    if opts.dry_run {
        for step in &plan.steps {
            writeln!(out, "{}", report::step(step, entry, opts.direction, fk))?;
        }
        print_kept(out, entry, plan, opts.direction)?;
        writeln!(out, "{}", report::DRY_RUN_NOTE)?;
    }
    Ok(())
}

fn print_kept(out: &mut dyn Write, entry: &Entry, plan: &plan::Plan, d: Direction) -> Result<()> {
    let missed: &[String] = match d {
        Direction::Undo => &entry.missed,
        Direction::Redo => &[],
    };
    if plan.kept.is_empty() && missed.is_empty() {
        return Ok(());
    }
    writeln!(out, "{}", report::kept_heading(d))?;
    for k in &plan.kept {
        writeln!(out, "{}", report::kept(k, entry, entry.forge))?;
    }
    for what in missed {
        writeln!(out, "{}", report::missed(what))?;
    }
    let open = plan
        .kept
        .iter()
        .filter(|k| matches!(k, Kept::OpenPr { .. }))
        .count();
    if open > 0 {
        writeln!(out, "{}", report::close_hint(open))?;
    }
    Ok(())
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

/// The checks that need only jj: is the entry finished, does jj still have
/// its operation, and is the repo exactly as the entry (or the last undo of
/// it) left it.
fn check_local(cx: &Context, entry: &Entry, opts: Options) -> Result<Option<Blocked>> {
    let direction = opts.direction;
    if cx.watch_running {
        return Ok(Some(Blocked::WatchRunning));
    }
    if entry.state == State::Running {
        return Ok(Some(Blocked::StillRunning));
    }
    if direction == Direction::Undo && !entry.local_undone && !entry.absorbed.is_empty() {
        return Ok(Some(Blocked::Absorbed(entry.absorbed.clone())));
    }
    let (expected, restore_to) = if entry.local_undone {
        (entry.undone_view.as_ref(), entry.end_op.as_ref())
    } else {
        (entry.end_view.as_ref(), Some(&entry.start_op))
    };
    let (Some(expected), Some(restore_to)) = (expected, restore_to) else {
        return Ok(Some(Blocked::NoEnd));
    };
    let restores = (direction == Direction::Undo) != entry.local_undone;
    if restores && !cx.repo.op_exists(restore_to)? {
        return Ok(Some(Blocked::OpGone(restore_to.clone())));
    }
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
        return Ok(Some(Blocked::RepoChanged { since }));
    }
    Ok(None)
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
