//! Carry out a [`Plan`] whole, or not at all. Each step taken yields its
//! inverse; when a step fails, the inverses run newest first, so the repo and
//! the forge end as they began. The journal is kept current after every step,
//! so a run that cannot even put itself back says exactly where it stopped,
//! and the next `jjpr undo` or `jjpr redo` plans from there.

use std::collections::HashMap;
use std::io::Write;

use anyhow::{Context, Result};

use crate::forge::Forge;

use super::failed;
use super::journal::{Action, Entry, Journal, State};
use super::plan::{Direction, Plan, Step};
use super::repo::UndoRepo;
use super::report;
use super::rollback;

pub struct Target<'a> {
    pub repo: &'a dyn UndoRepo,
    pub forge: &'a dyn Forge,
    pub journal: &'a Journal,
}

/// A run that failed at a step.
#[derive(Debug)]
pub struct Stopped {
    pub cause: anyhow::Error,
    /// `Ok` once every step taken was put back; otherwise why one could not be.
    pub put_back: Result<()>,
}

/// Where the local repo stood when the run began, to put it back to.
struct Start {
    op: String,
    view: Option<String>,
    local_undone: bool,
}

pub fn run(
    t: &Target,
    entry: &mut Entry,
    plan: &Plan,
    direction: Direction,
    out: &mut dyn Write,
) -> Result<(), Stopped> {
    let undo = direction == Direction::Undo;
    let start = Start {
        op: t.repo.current_op().map_err(|cause| Stopped {
            cause,
            put_back: Ok(()),
        })?,
        view: t.repo.view_fingerprint().ok(),
        local_undone: entry.local_undone,
    };
    let mut renames = Renames::new();
    // The inverse of each forge or remote step taken, oldest first. The local
    // restore is put back by returning to `start`.
    let mut taken: Vec<Step> = Vec::new();
    for step in &plan.steps {
        say(out, &report::step(step, entry, direction, entry.forge));
        if let Err(cause) = take(t, entry, step, undo, &mut renames, &mut taken) {
            let put_back = put_back(t, entry, &taken, &start, direction, out);
            if let Err(e) = settle(t, entry) {
                eprintln!("  Warning: could not update the undo journal: {e:#}");
            }
            return Err(Stopped { cause, put_back });
        }
    }
    for record in &mut entry.actions {
        record.undone = undo;
    }
    if let Err(e) = settle(t, entry) {
        eprintln!("  Warning: could not update the undo journal: {e:#}");
    }
    Ok(())
}

fn say(out: &mut dyn Write, line: &str) {
    // A closed stdout must not stop a run halfway.
    let _ = writeln!(out, "{line}");
}

/// Take one step towards `undone`, noting its inverse once it has gone
/// through, then mark its record.
fn take(
    t: &Target,
    entry: &mut Entry,
    step: &Step,
    undone: bool,
    renames: &mut Renames,
    taken: &mut Vec<Step>,
) -> Result<()> {
    let step = resolved(step, renames);
    let posted = match apply(t, entry, &step, undone, renames) {
        Ok(posted) => posted,
        Err(e) => {
            if landed(t, &step) {
                taken.push(rollback::inverse(&step, "", None));
            }
            return Err(e);
        }
    };
    if !matches!(step, Step::Local { .. }) {
        taken.push(rollback::inverse(&step, "", posted));
    }
    if let Some(record) = record_of(&step) {
        entry.actions[record].undone = undone;
    }
    t.journal.save(entry)
}

/// Whether a step that reported failure reached the remote all the same: a
/// push whose local clean-up failed after the push itself went through.
fn landed(t: &Target, step: &Step) -> bool {
    match step {
        Step::Push {
            bookmark,
            remote,
            to,
            ..
        } => t
            .repo
            .targets(bookmark, remote)
            .is_ok_and(|now| now.remote == *to),
        _ => false,
    }
}

/// Run the inverses of the steps taken, newest first, then put the local
/// repo back where the run found it. `direction` is the failed run's.
fn put_back(
    t: &Target,
    entry: &mut Entry,
    taken: &[Step],
    start: &Start,
    direction: Direction,
    out: &mut dyn Write,
) -> Result<()> {
    let moved = |t: &Target| t.repo.view_fingerprint().ok() != start.view;
    if taken.is_empty() && !moved(t) && entry.local_undone == start.local_undone {
        return Ok(());
    }
    say(out, &failed::putting_back(direction));
    let undo = direction == Direction::Undo;
    let mut renames = Renames::new();
    let mut ignored = Vec::new();
    for step in taken.iter().rev() {
        say(out, &failed::back_step(step, entry.forge));
        take(t, entry, step, !undo, &mut renames, &mut ignored)?;
    }
    if moved(t) {
        let local = Step::Local {
            op: start.op.clone(),
        };
        say(out, &failed::back_step(&local, entry.forge));
        t.repo.restore_repo_only(&start.op)?;
    }
    entry.local_undone = start.local_undone;
    t.journal.save(entry)
}

/// Record where the repo stands now and what state the entry is in.
fn settle(t: &Target, entry: &mut Entry) -> Result<()> {
    let view = t.repo.view_fingerprint().ok();
    if entry.local_undone {
        entry.undone_view = view;
    } else {
        entry.end_view = view;
    }
    entry.last_op = t.repo.current_op().ok();
    let undone = entry.actions.iter().filter(|r| r.undone).count();
    entry.state = match (entry.local_undone, undone) {
        (true, n) if n == entry.actions.len() => State::Undone,
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

/// `step` with the comment ids this run has posted again under new ones.
fn resolved(step: &Step, renames: &Renames) -> Step {
    let mut step = step.clone();
    match &mut step {
        Step::DeleteComment { pr, id, .. }
        | Step::EditComment { pr, id, .. }
        | Step::PostComment { pr, id, .. } => *id = renamed(renames, *pr, *id),
        _ => {}
    }
    step
}

/// Carry out one step. Returns the id of a comment it posted.
fn apply(
    t: &Target,
    entry: &mut Entry,
    step: &Step,
    undone: bool,
    renames: &mut Renames,
) -> Result<Option<u64>> {
    let (owner, repo, forge) = (entry.owner.clone(), entry.repo.clone(), t.forge);
    match step {
        Step::Local { op } => {
            t.repo.restore_repo_only(op)?;
            entry.local_undone = undone;
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
        Step::DeleteComment { id, .. } => forge.delete_comment(&owner, &repo, *id)?,
        Step::EditComment { id, to, .. } => forge.update_comment(&owner, &repo, *id, to)?,
        Step::PostComment { pr, id, body, .. } => {
            let posted = forge.create_comment(&owner, &repo, *pr, body)?;
            renames.insert((*pr, *id), posted.id);
            rename_comment(t, entry, *pr, *id, posted.id);
            return Ok(Some(posted.id));
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
    Ok(None)
}

/// Every record naming the comment `old`, in this entry and in others (one
/// created it, the next edited it), finds it under `new` from now on.
fn rename_comment(t: &Target, entry: &mut Entry, pr: u64, old: u64, new: u64) {
    for r in &mut entry.actions {
        if let Action::CommentCreate { pr: p, id, .. }
        | Action::CommentUpdate { pr: p, id, .. }
        | Action::CommentDelete { pr: p, id, .. } = &mut r.action
            && *p == pr
            && *id == old
        {
            *id = new;
        }
    }
    if let Err(e) = t.journal.rewrite_comment(pr, old, new) {
        eprintln!("  Warning: could not note the stack comment's new id: {e:#}");
    }
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

/// The executor against an in-memory repo and forge whose n-th effect fails:
/// a run either completes or leaves everything as it found it.
#[cfg(test)]
mod all_or_nothing {
    use std::collections::{BTreeMap, HashMap};

    use proptest::prelude::*;

    use super::*;
    use crate::forge::ForgeKind;
    use crate::undo::journal::{Record, SCHEMA};
    use crate::undo::plan::{Observed, SeenPr, Status};
    use crate::undo::rollback::differences;
    use crate::undo::world::{self, Failing, World};

    const COMMITS: [&str; 3] = ["c0", "c1", "c2"];
    const BOOKMARKS: [&str; 3] = ["b0", "b1", "b2"];

    fn entry(steps: usize, local_undone: bool, undone: bool) -> Entry {
        // As `settle` reads the marks: with no records, only the local restore counts.
        let (all_undone, none_undone) = (undone || steps == 0, !undone || steps == 0);
        Entry {
            schema: SCHEMA,
            id: "1-1".into(),
            command: "submit".into(),
            started_at: 0,
            remote: "origin".into(),
            forge: ForgeKind::GitHub,
            owner: "o".into(),
            repo: "r".into(),
            start_op: "0".into(),
            end_op: Some("1".into()),
            end_view: None,
            absorbed: vec![],
            state: match (local_undone, all_undone, none_undone) {
                (true, true, _) => State::Undone,
                (false, _, true) => State::Done,
                _ => State::PartlyUndone,
            },
            local_undone,
            undone_view: None,
            last_op: None,
            closed_by_push: Vec::new(),
            missed: Vec::new(),
            actions: (0..steps)
                .map(|_| Record {
                    action: Action::Ready { number: 0 },
                    confirmed: true,
                    undone,
                })
                .collect(),
        }
    }

    /// One valid step against `forge` as it stands, chosen by `kind` and `pick`.
    fn step_for(forge: &Observed, record: usize, kind: u8, pick: usize) -> Step {
        let numbers = world::sorted(&forge.prs);
        let number = numbers[pick % numbers.len()];
        let pr = &forge.prs[&number];
        let other = |now: &str, from: &[&str]| -> String {
            let options: Vec<&&str> = from.iter().filter(|c| **c != now).collect();
            options[pick % options.len()].to_string()
        };
        let comments: Vec<u64> = forge
            .comments
            .get(&number)
            .map(world::sorted)
            .unwrap_or_default();
        match kind % 8 {
            0 => {
                let bookmark = BOOKMARKS[pick % BOOKMARKS.len()];
                let from = forge.branches.get(bookmark).cloned().flatten();
                let to = match &from {
                    Some(_) if pick.is_multiple_of(4) => None,
                    Some(c) => Some(other(c, &COMMITS)),
                    None => Some(COMMITS[pick % COMMITS.len()].to_string()),
                };
                Step::Push {
                    record,
                    bookmark: bookmark.into(),
                    remote: "origin".into(),
                    from,
                    to,
                }
            }
            1 if pr.status == Status::Open => Step::Close { record, number },
            1 => Step::Reopen { record, number },
            2 => Step::Base {
                record,
                number,
                from: pr.base.clone(),
                to: other(&pr.base, &["main", "b0", "b1"]),
            },
            3 => Step::Body {
                record,
                number,
                from: pr.body.clone(),
                to: other(&pr.body, &["x", "y", "z"]),
            },
            4 if pr.draft => Step::Ready { record, number },
            4 => Step::Draft { record, number },
            5 if !pr.reviewers.is_empty() => Step::Unrequest {
                record,
                number,
                who: vec![pr.reviewers[0].clone()],
            },
            5 => Step::Request {
                record,
                number,
                who: vec!["carol".into()],
            },
            6 if !comments.is_empty() => {
                let id = comments[pick % comments.len()];
                Step::DeleteComment {
                    record,
                    pr: number,
                    id,
                    body: forge.comments[&number][&id].clone(),
                }
            }
            7 if !comments.is_empty() => {
                let id = comments[pick % comments.len()];
                let now = forge.comments[&number][&id].clone();
                Step::EditComment {
                    record,
                    pr: number,
                    id,
                    to: other(&now, &["s1", "s2", "s3"]),
                    from: now,
                }
            }
            _ => Step::PostComment {
                record,
                pr: number,
                id: 900 + record as u64,
                body: "posted".into(),
            },
        }
    }

    fn seen_pr() -> impl Strategy<Value = SeenPr> {
        (
            prop::sample::select(vec!["main", "b0", "b1"]),
            prop::sample::select(vec!["x", "y"]),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(base, body, draft, open, reviewed)| SeenPr {
                base: base.into(),
                body: body.into(),
                draft,
                status: if open { Status::Open } else { Status::Closed },
                reviewers: if reviewed {
                    vec!["alice".into()]
                } else {
                    vec![]
                },
            })
    }

    prop_compose! {
        fn forge_state()(
            heads in prop::collection::vec(prop::option::of(prop::sample::select(COMMITS.to_vec())), 3),
            prs in prop::collection::vec(seen_pr(), 1..4),
            comment_counts in prop::collection::vec(0usize..3, 3),
        ) -> Observed {
            let mut forge = Observed::default();
            for (b, head) in BOOKMARKS.iter().zip(heads) {
                forge.branches.insert(b.to_string(), head.map(Into::into));
            }
            let mut id = 1;
            for (i, pr) in prs.into_iter().enumerate() {
                let number = i as u64 + 1;
                let mut comments = HashMap::new();
                for _ in 0..comment_counts[i % 3] {
                    comments.insert(id, format!("s{}", id % 2 + 1));
                    id += 1;
                }
                forge.comments.insert(number, comments);
                forge.prs.insert(number, pr);
            }
            forge
        }
    }

    struct Case {
        direction: Direction,
        world: World,
        entry: Entry,
        plan: Plan,
        before: Observed,
        local_before: BTreeMap<String, String>,
        /// The forge once every step has run.
        after: Observed,
        _dir: tempfile::TempDir,
        journal: Journal,
    }

    fn case(forge: Observed, choices: &[(u8, usize)], with_local: bool, redo: bool) -> Case {
        let local: BTreeMap<String, String> = BOOKMARKS
            .iter()
            .map(|b| (b.to_string(), "c0".into()))
            .collect();
        let world = World::new(local, forge.clone());
        // The command's own operation: the local restore goes back past it.
        world.set_bookmark("b0", "c2").unwrap();
        let mut steps = Vec::new();
        if with_local {
            steps.push(Step::Local { op: "0".into() });
        }
        let mut after = forge.clone();
        for (record, &(kind, pick)) in choices.iter().enumerate() {
            let step = step_for(&after, record, kind, pick);
            world::apply(&mut after, &step);
            steps.push(step);
        }
        let dir = tempfile::TempDir::new().unwrap();
        let journal = Journal::at(dir.path().to_path_buf());
        Case {
            local_before: world.local(),
            world,
            entry: entry(choices.len(), with_local == redo, redo),
            direction: if redo {
                Direction::Redo
            } else {
                Direction::Undo
            },
            plan: Plan {
                steps,
                ..Plan::default()
            },
            before: forge,
            after,
            _dir: dir,
            journal,
        }
    }

    fn run_case(c: &mut Case) -> Result<(), Stopped> {
        let t = Target {
            repo: &c.world,
            forge: &c.world,
            journal: &c.journal,
        };
        run(&t, &mut c.entry, &c.plan, c.direction, &mut Vec::new())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Whichever effect fails, an undo or a redo puts back every step it took: the
        /// forge, the local repo and the journal's marks end as they began.
        /// With no failure, every step lands and every record is marked.
        #[test]
        fn a_run_completes_or_changes_nothing(
            forge in forge_state(),
            choices in prop::collection::vec((any::<u8>(), any::<usize>()), 0..8),
            with_local in any::<bool>(),
            redo in any::<bool>(),
            fail in 0usize..20,
        ) {
            let mut c = case(forge, &choices, with_local, redo);
            let state_before = c.entry.state;
            let local_before = c.entry.local_undone;
            c.world.fail(Failing { at: Some(fail), from: None });
            match run_case(&mut c) {
                Err(stopped) => {
                    prop_assert!(stopped.put_back.is_ok(), "{:?} after {:?}", stopped.put_back, stopped.cause);
                    prop_assert_eq!(differences(&c.before, &c.world.forge()), vec![]);
                    prop_assert_eq!(c.world.local(), c.local_before.clone());
                    prop_assert!(c.entry.actions.iter().all(|r| r.undone == redo));
                    prop_assert_eq!(c.entry.local_undone, local_before);
                    prop_assert_eq!(c.entry.state, state_before);
                }
                Ok(()) => {
                    prop_assert_eq!(differences(&c.after, &c.world.forge()), vec![]);
                    prop_assert!(c.entry.actions.iter().all(|r| r.undone != redo));
                    prop_assert_eq!(c.entry.local_undone, !redo);
                    prop_assert_eq!(c.entry.state, if redo { State::Done } else { State::Undone });
                }
            }
            let saved = c.journal.load().unwrap().entries;
            prop_assert_eq!(saved.len(), 1);
            prop_assert_eq!(&saved[0].actions, &c.entry.actions, "the journal says what happened");
        }
    }

    /// When putting back fails too, the journal marks exactly the steps that
    /// still stand, so the next undo or redo plans from there.
    #[test]
    fn a_put_back_that_fails_leaves_the_record_true_to_the_forge() {
        let mut forge = Observed::default();
        forge.prs.insert(
            1,
            SeenPr {
                base: "main".into(),
                body: "x".into(),
                status: Status::Open,
                ..SeenPr::default()
            },
        );
        // Body, base, then close: the close fails, and so does every effect after.
        let mut c = case(forge, &[(3, 0), (2, 0), (1, 0)], false, false);
        c.world.fail(Failing {
            at: None,
            from: Some(2),
        });
        let mut out = Vec::new();
        let t = Target {
            repo: &c.world,
            forge: &c.world,
            journal: &c.journal,
        };
        let stopped = run(&t, &mut c.entry, &c.plan, Direction::Undo, &mut out).unwrap_err();
        assert!(stopped.cause.to_string().contains("effect 2"));
        assert!(
            stopped
                .put_back
                .unwrap_err()
                .to_string()
                .contains("effect 3")
        );
        let marks: Vec<bool> = c.entry.actions.iter().map(|r| r.undone).collect();
        assert_eq!(marks, vec![true, true, false]);
        assert_eq!(c.entry.state, State::PartlyUndone);
        assert_eq!(c.world.forge().prs[&1].body, "y");
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("That step failed. Putting back what this undo changed:"),
            "{out}"
        );
    }
}
