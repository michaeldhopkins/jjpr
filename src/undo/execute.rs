//! Carry out a [`Plan`], keeping the journal current after every step so a
//! run that stops partway can be picked up again.

use std::collections::HashMap;
use std::io::Write;

use anyhow::{Context, Result};

use crate::forge::Forge;

use super::journal::{Action, Entry, Journal, State};
use super::plan::{Direction, Plan, Step};
use super::repo::UndoRepo;
use super::report;

pub struct Target<'a> {
    pub repo: &'a dyn UndoRepo,
    pub forge: &'a dyn Forge,
    pub journal: &'a Journal,
}

pub fn run(
    t: &Target,
    entry: &mut Entry,
    plan: &Plan,
    direction: Direction,
    out: &mut dyn Write,
) -> Result<()> {
    let undo = direction == Direction::Undo;
    let mut renames = Renames::new();
    for step in &plan.steps {
        writeln!(out, "{}", report::step(step, entry, direction, entry.forge))?;
        let result = apply(t, entry, step, undo, &mut renames);
        // GitHub will not reopen a PR whose branch moved while it was closed,
        // which is what undoing the push that closed it does. The rest of the
        // undo still stands, so say so and go on.
        if let (Err(e), Step::Reopen { .. }, true) = (&result, step, undo) {
            writeln!(out, "  Warning: could not reopen it: {e:#}")?;
            continue;
        }
        if let Err(e) = result {
            settle(t, entry, &[])?;
            let verb = if undo { "undo" } else { "redo" };
            return Err(e.context(format!(
                "stopped partway; what is left is still recorded, and `jjpr {verb}` picks it up"
            )));
        }
        if let Some(record) = record_of(step) {
            entry.actions[record].undone = undo;
        }
        t.journal.save(entry)?;
    }
    for (i, record) in entry.actions.iter_mut().enumerate() {
        if record.undone != undo && !plan.left.contains(&i) {
            record.undone = undo;
        }
    }
    settle(t, entry, &plan.left)
}

/// Record where the repo stands now and what state the entry is in. `left` are
/// the records this run left for `--force` on purpose.
fn settle(t: &Target, entry: &mut Entry, left: &[usize]) -> Result<()> {
    let view = t.repo.view_fingerprint().ok();
    if entry.local_undone {
        entry.undone_view = view;
    } else {
        entry.end_view = view;
    }
    entry.last_op = t.repo.current_op().ok();
    let undone = entry.actions.iter().filter(|r| r.undone).count();
    let only_kept = entry
        .actions
        .iter()
        .enumerate()
        .all(|(i, r)| r.undone || left.contains(&i));
    entry.state = match (entry.local_undone, undone) {
        (true, n) if n == entry.actions.len() => State::Undone,
        (true, _) if !left.is_empty() && only_kept => State::KeptOpen,
        (false, 0) => State::Done,
        _ => State::PartlyUndone,
    };
    t.journal.save(entry)
}

fn record_of(step: &Step) -> Option<usize> {
    match step {
        Step::Local { .. } => None,
        Step::Push { record, .. }
        | Step::Reopen { record, .. }
        | Step::Close { record, .. }
        | Step::Base { record, .. }
        | Step::DeleteComment { record, .. }
        | Step::EditComment { record, .. }
        | Step::PostComment { record, .. }
        | Step::Body { record, .. }
        | Step::Draft { record, .. }
        | Step::Ready { record, .. }
        | Step::Unrequest { record, .. }
        | Step::Request { record, .. } => Some(*record),
    }
}

/// Comment ids this run posted again: (PR, old id) to the new one.
type Renames = HashMap<(u64, u64), u64>;

fn renamed(renames: &Renames, pr: u64, id: u64) -> u64 {
    renames.get(&(pr, id)).copied().unwrap_or(id)
}

fn apply(
    t: &Target,
    entry: &mut Entry,
    step: &Step,
    undo: bool,
    renames: &mut Renames,
) -> Result<()> {
    let (owner, repo, forge) = (entry.owner.clone(), entry.repo.clone(), t.forge);
    match step {
        Step::Local { op } => {
            t.repo.restore_repo_only(op)?;
            entry.local_undone = undo;
            t.journal.save(entry)?;
        }
        Step::Push {
            bookmark,
            remote,
            to,
            ..
        } => push(t.repo, bookmark, remote, to.as_deref())?,
        Step::Reopen { number, .. } => forge.reopen_pr(&owner, &repo, *number)?,
        Step::Close { number, .. } => forge.close_pr(&owner, &repo, *number)?,
        Step::Base { number, to, .. } => forge.update_pr_base(&owner, &repo, *number, to)?,
        Step::DeleteComment { pr, id, .. } => {
            forge.delete_comment(&owner, &repo, renamed(renames, *pr, *id))?;
        }
        Step::EditComment { pr, id, to, .. } => {
            forge.update_comment(&owner, &repo, renamed(renames, *pr, *id), to)?;
        }
        Step::PostComment { pr, id, body, .. } => {
            let posted = forge.create_comment(&owner, &repo, *pr, body)?;
            let old = renamed(renames, *pr, *id);
            renames.insert((*pr, *id), posted.id);
            // Every record naming the comment, in this entry and in others
            // (one created it, the next edited it), must find it under its
            // new id from now on.
            for r in &mut entry.actions {
                if let Action::CommentCreate { pr: p, id, .. }
                | Action::CommentUpdate { pr: p, id, .. }
                | Action::CommentDelete { pr: p, id, .. } = &mut r.action
                    && *p == *pr
                    && *id == old
                {
                    *id = posted.id;
                }
            }
            if let Err(e) = t.journal.rewrite_comment(*pr, old, posted.id) {
                eprintln!("  Warning: could not note the stack comment's new id: {e:#}");
            }
        }
        Step::Body { number, to, .. } => forge.update_pr_body(&owner, &repo, *number, to)?,
        Step::Draft { number, .. } => forge.convert_to_draft(&owner, &repo, *number)?,
        Step::Ready { number, .. } => forge.mark_pr_ready(&owner, &repo, *number)?,
        Step::Unrequest { number, who, .. } => {
            forge.remove_reviewers(&owner, &repo, *number, who)?;
        }
        Step::Request { number, who, .. } => {
            forge.request_reviewers(&owner, &repo, *number, who)?;
        }
    }
    Ok(())
}

/// Make the remote's `bookmark` point at `to` (or delete it) without leaving
/// the local repo changed. jj pushes only what a local bookmark says, so the
/// bookmark moves for the push, and the operation before the move is restored
/// afterwards (`--what repo`, which keeps the remote-tracking ref the push
/// just updated). Moving a bookmark onto a hidden commit makes it visible
/// again; the restore hides it once more.
fn push(repo: &dyn UndoRepo, bookmark: &str, remote: &str, to: Option<&str>) -> Result<()> {
    let before = repo.current_op()?;
    let local = repo.targets(bookmark, remote)?.local;
    let moved = match to {
        Some(commit) if local.as_deref() != Some(commit) => repo.set_bookmark(bookmark, commit),
        None if local.is_some() => repo.delete_bookmark(bookmark),
        _ => Ok(()),
    };
    let pushed = moved.and_then(|()| repo.push_bookmark(bookmark, remote));
    if repo.current_op()? != before {
        repo.restore_repo_only(&before)
            .context("failed to put the local bookmark back after pushing")?;
    }
    pushed
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::undo::repo::{Operation, Targets};

    /// Logs every call; `fail_push` makes the push fail.
    #[derive(Default)]
    struct Log {
        calls: Mutex<Vec<String>>,
        local: Option<String>,
        fail_push: bool,
        op: Mutex<u32>,
    }

    impl Log {
        fn call(&self, s: String) {
            self.calls.lock().unwrap().push(s);
        }
        fn bump(&self) {
            *self.op.lock().unwrap() += 1;
        }
    }

    impl UndoRepo for Log {
        fn current_op(&self) -> Result<String> {
            Ok(format!("op{}", self.op.lock().unwrap()))
        }
        fn snapshot(&self) -> Result<()> {
            Ok(())
        }
        fn view_fingerprint(&self) -> Result<String> {
            Ok(String::new())
        }
        fn view_fingerprint_at(&self, _: &str) -> Result<String> {
            Ok(String::new())
        }
        fn ops_since(&self, _: &str, _: usize) -> Result<Option<Vec<Operation>>> {
            Ok(None)
        }
        fn op_exists(&self, _: &str) -> Result<bool> {
            Ok(true)
        }
        fn restore_repo_only(&self, op: &str) -> Result<()> {
            self.call(format!("restore {op}"));
            self.bump();
            Ok(())
        }
        fn targets(&self, _: &str, _: &str) -> Result<Targets> {
            Ok(Targets {
                local: self.local.clone(),
                remote: None,
            })
        }
        fn set_bookmark(&self, b: &str, c: &str) -> Result<()> {
            self.call(format!("set {b} {c}"));
            self.bump();
            Ok(())
        }
        fn delete_bookmark(&self, b: &str) -> Result<()> {
            self.call(format!("delete {b}"));
            self.bump();
            Ok(())
        }
        fn push_bookmark(&self, b: &str, r: &str) -> Result<()> {
            self.call(format!("push {b} {r}"));
            if self.fail_push {
                anyhow::bail!("stale info");
            }
            self.bump();
            Ok(())
        }
    }

    fn calls(log: &Log) -> Vec<String> {
        log.calls.lock().unwrap().clone()
    }

    #[test]
    fn a_push_moves_the_bookmark_pushes_and_puts_the_repo_back() {
        let log = Log {
            local: Some("mine".into()),
            ..Log::default()
        };
        push(&log, "a", "origin", Some("old")).unwrap();
        assert_eq!(
            calls(&log),
            vec!["set a old", "push a origin", "restore op0"]
        );
    }

    #[test]
    fn a_deletion_deletes_the_local_bookmark_only_when_there_is_one() {
        let log = Log {
            local: Some("mine".into()),
            ..Log::default()
        };
        push(&log, "a", "origin", None).unwrap();
        assert_eq!(
            calls(&log),
            vec!["delete a", "push a origin", "restore op0"]
        );
        let log = Log::default();
        push(&log, "a", "origin", None).unwrap();
        assert_eq!(calls(&log), vec!["push a origin", "restore op0"]);
    }

    #[test]
    fn a_bookmark_already_there_is_pushed_without_moving() {
        let log = Log {
            local: Some("new".into()),
            ..Log::default()
        };
        push(&log, "a", "origin", Some("new")).unwrap();
        assert_eq!(calls(&log), vec!["push a origin", "restore op0"]);
    }

    #[test]
    fn a_failed_push_still_puts_the_repo_back() {
        let log = Log {
            local: Some("mine".into()),
            fail_push: true,
            ..Log::default()
        };
        let err = push(&log, "a", "origin", Some("old")).unwrap_err();
        assert!(err.to_string().contains("stale info"));
        assert_eq!(
            calls(&log),
            vec!["set a old", "push a origin", "restore op0"]
        );
    }
}
