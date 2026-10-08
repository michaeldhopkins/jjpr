//! `jjpr undo` twice, over jj work done since the command.
//!
//! Undoing a command restores the repo to before it, which would discard a
//! `jj describe` or an amend made since, so that alone refuses the undo. When
//! that is the only thing in the way, and the work since is local to this
//! workspace, the first `jjpr undo` takes back just that work, recorded as an
//! entry of its own, so `jjpr redo` puts it back. The next `jjpr undo` finds
//! the repo as the command left it. The design is in
//! `notes/undo-redo-design.md`, section 2.

use std::io::Write;

use anyhow::Result;

use super::journal::{Entry, SCHEMA, State, new_id};
use super::plan::{Blocker, Direction, Plan, Step};
use super::repo::{Operation, UndoRepo};
use super::{Context, Options, execute, explain, failed, report};

/// The `command` of an entry that steps back over jj work.
pub const COMMAND: &str = "jj";

/// What the work since allows.
#[derive(Debug, PartialEq, Eq)]
pub enum Assess {
    /// Local to this workspace; `files` would leave the disk.
    Allowed {
        files: Vec<String>,
    },
    Refused(Blocker),
}

/// Whether an operation reached a remote: a fetch or a push.
pub fn remote_op(description: &str) -> bool {
    // jj describes a fetch as "fetch from git remote(s) …" and every push as
    // "push … to git remote …".
    description.starts_with("fetch from git remote") || description.contains(" to git remote")
}

/// Whether the work after `op` (`since`, newest first) may be stepped back over.
pub fn assess(repo: &dyn UndoRepo, op: &str, since: &[Operation]) -> Result<Assess> {
    if let Some(o) = since.iter().find(|o| remote_op(&o.description)) {
        return Ok(Assess::Refused(Blocker::NotLocal {
            what: format!("jj operation {} ({})", report::short(&o.id), o.description),
        }));
    }
    let then = repo.working_copies(Some(op))?;
    let now = repo.working_copies(None)?;
    let mine = repo.own_working_copies()?;
    let others = |pairs: &[(String, String)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .filter(|(name, _)| !mine.contains(name))
            .cloned()
            .collect()
    };
    let (before, after) = (others(&then), others(&now));
    if before != after {
        let name = after
            .iter()
            .chain(before.iter())
            .find(|p| !before.contains(p) || !after.contains(p))
            .map(|(name, _)| name.clone())
            .unwrap_or_default();
        return Ok(Assess::Refused(Blocker::NotLocal {
            what: format!("workspace '{name}' changed"),
        }));
    }
    Ok(Assess::Allowed {
        files: repo.files_changed_since(op)?,
    })
}

/// Step back over the work since `entry` when that is all that stands in the
/// way. Returns whether it did (or, in a dry run, would have), in which case
/// the run is over. Otherwise `blockers` say why not.
pub(super) fn try_it(
    cx: &Context,
    entry: &Entry,
    blockers: &mut Vec<Blocker>,
    opts: Options,
    out: &mut dyn Write,
) -> Result<bool> {
    let Some(at) = blockers
        .iter()
        .position(|b| matches!(b, Blocker::RepoChanged { .. }))
    else {
        return Ok(false);
    };
    let hard = blockers
        .iter()
        .any(|b| !b.forceable() && !matches!(b, Blocker::RepoChanged { .. }));
    let since = match &blockers[at] {
        Blocker::RepoChanged { since } if !since.is_empty() => since.clone(),
        _ => return Ok(false),
    };
    let Some(op) = entry.last_op.clone().or_else(|| entry.end_op.clone()) else {
        return Ok(false);
    };
    if hard {
        return Ok(false);
    }
    let files = match assess(cx.repo, &op, &since)? {
        Assess::Refused(b) => {
            blockers.push(b);
            return Ok(false);
        }
        Assess::Allowed { files } => files,
    };
    blockers.remove(at);
    if !files.is_empty() && !opts.force {
        blockers.push(Blocker::EditsOnDisk { files });
        return Ok(false);
    }
    let name = report::name(entry, cx.now);
    if opts.dry_run {
        writeln!(
            out,
            "{}",
            explain::stepping_back(&since, &name, &entry.command, true)
        )?;
        writeln!(out, "{}", report::DRY_RUN_NOTE)?;
        return Ok(true);
    }
    let mut step = record(cx, entry, &op)?;
    cx.journal.save(&step)?;
    let plan = Plan {
        steps: vec![Step::Local { op }],
        ..Plan::default()
    };
    let target = execute::Target {
        repo: cx.repo,
        forge: None,
        journal: cx.journal,
    };
    if let Err(stopped) = execute::run(&target, &mut step, &plan, Direction::Undo, &mut Vec::new())
    {
        let cause = format!("{:#}", stopped.cause);
        let text = match &stopped.put_back {
            Err(e) => {
                failed::stopped_partway(&step, Direction::Undo, cx.now, &cause, &format!("{e:#}"))
            }
            Ok(()) => failed::stopped_and_put_back(&step, Direction::Undo, cx.now, &cause, &[]),
        };
        anyhow::bail!("{text}");
    }
    writeln!(
        out,
        "{}",
        explain::stepping_back(&since, &name, &entry.command, false)
    )?;
    Ok(true)
}

/// The entry for a step back to `op`, from the repo as it is now.
fn record(cx: &Context, of: &Entry, op: &str) -> Result<Entry> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(Entry {
        schema: SCHEMA,
        id: new_id(nanos, std::process::id()),
        command: COMMAND.to_string(),
        started_at: cx.now,
        remote: of.remote.clone(),
        forge: of.forge,
        owner: of.owner.clone(),
        repo: of.repo.clone(),
        start_op: op.to_string(),
        end_op: Some(cx.repo.current_op()?),
        end_view: Some(cx.repo.view_fingerprint()?),
        absorbed: Vec::new(),
        state: State::Done,
        local_undone: false,
        undone_view: None,
        last_op: None,
        closed_by_push: Vec::new(),
        missed: Vec::new(),
        actions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::remote_op;

    #[test]
    fn fetches_and_pushes_reach_a_remote_and_local_work_does_not() {
        for d in [
            "fetch from git remote(s) origin",
            "push all tracked bookmarks to git remote origin",
            "push bookmark auth to git remote origin",
        ] {
            assert!(remote_op(d), "{d}");
        }
        for d in [
            "describe commit 613b9b05",
            "snapshot working copy",
            "new empty commit",
            "rebase commit 1a2b and descendants",
            "import git refs",
            "squash commits into 5d6e",
        ] {
            assert!(!remote_op(d), "{d}");
        }
    }
}
