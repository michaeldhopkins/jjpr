//! Every line `jjpr undo` and `jjpr redo` print, in one place.

use crate::forge::ForgeKind;

use super::journal::{Action, Entry, State};
use super::plan::{Changed, Direction, Kept, Refusal, Step, touched_prs};
use super::repo::Operation;

/// "`jjpr submit` from 14:02".
pub fn name(entry: &Entry, now: u64) -> String {
    format!(
        "`jjpr {}` from {}",
        entry.command,
        when(entry.started_at, now)
    )
}

fn verb(direction: Direction) -> &'static str {
    match direction {
        Direction::Undo => "undo",
        Direction::Redo => "redo",
    }
}

pub fn header(entry: &Entry, direction: Direction, dry_run: bool, now: u64) -> String {
    let doing = match (direction, dry_run) {
        (Direction::Undo, true) => "Would undo",
        (Direction::Redo, true) => "Would redo",
        (Direction::Undo, false) => "Undoing",
        (Direction::Redo, false) => "Redoing",
    };
    format!("{doing} {}:", name(entry, now))
}

fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn commit(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn step(step: &Step, entry: &Entry, direction: Direction, fk: ForgeKind) -> String {
    let undo = direction == Direction::Undo;
    let pr = |n: &u64| fk.format_ref(*n);
    let cmd = &entry.command;
    let text = match step {
        Step::Local { op } => {
            let side = if undo { "before" } else { "after" };
            format!(
                "Restore the local repo to operation {}, from {side} the {cmd}",
                short(op)
            )
        }
        Step::Push {
            bookmark,
            remote,
            to: None,
            ..
        } => format!("Delete branch '{bookmark}' from {remote}"),
        Step::Push {
            bookmark,
            remote,
            from: None,
            to: Some(_),
            ..
        } => format!("Push '{bookmark}' to {remote} again"),
        Step::Push {
            bookmark,
            to: Some(to),
            ..
        } if undo => format!("Force-push '{bookmark}' back to {}", commit(to)),
        Step::Push {
            bookmark,
            to: Some(to),
            ..
        } => format!("Force-push '{bookmark}' to {} again", commit(to)),
        Step::Reopen { number, .. } if undo => {
            format!("Reopen {}, which the push closed", pr(number))
        }
        Step::Reopen { number, .. } => format!("Reopen {}", pr(number)),
        Step::Close { number, .. } => format!("Close {}, which the {cmd} opened", pr(number)),
        Step::Base {
            number, from, to, ..
        } if undo => format!("Retarget {} from '{from}' back to '{to}'", pr(number)),
        Step::Base {
            number, from, to, ..
        } => format!("Retarget {} from '{from}' to '{to}' again", pr(number)),
        Step::DeleteComment { pr: n, .. } => format!("Delete the stack comment on {}", pr(n)),
        Step::EditComment { pr: n, .. } if undo => {
            format!("Restore the stack comment on {}", pr(n))
        }
        Step::EditComment { pr: n, .. } => format!("Rewrite the stack comment on {}", pr(n)),
        Step::PostComment { pr: n, .. } if undo => {
            format!("Put back the stack comment on {}", pr(n))
        }
        Step::PostComment { pr: n, .. } => format!("Post the stack comment on {} again", pr(n)),
        Step::Body { number, .. } if undo => format!("Restore the description of {}", pr(number)),
        Step::Body { number, .. } => format!("Rewrite the description of {}", pr(number)),
        Step::Draft { number, .. } => format!("Mark {} as a draft again", pr(number)),
        Step::Ready { number, .. } => format!("Mark {} as ready for review again", pr(number)),
        Step::Unrequest { number, who, .. } => format!(
            "Withdraw the review request to {} on {}",
            who.join(", "),
            pr(number)
        ),
        Step::Request { number, who, .. } => format!(
            "Request a review from {} on {} again",
            who.join(", "),
            pr(number)
        ),
    };
    format!("  {text}")
}

/// The lines under "Not undone:".
pub fn kept(kept: &Kept, entry: &Entry, fk: ForgeKind) -> String {
    match kept {
        Kept::OpenPr { number } => {
            let head = entry.actions.iter().find_map(|r| match &r.action {
                Action::CreatePr { number: n, head } if n == number => Some(head.as_str()),
                _ => None,
            });
            // The branch is named only when the command pushed it new, so
            // that `--force` would delete it too.
            let pushed_new = |b: &str| {
                entry.actions.iter().any(|r| {
                    matches!(&r.action, Action::Push { bookmark, before: None, .. } if bookmark == b)
                })
            };
            match head.filter(|b| pushed_new(b)) {
                Some(b) => format!(
                    "  {} and its branch '{b}' stay open: closing a PR needs --force",
                    fk.format_ref(*number)
                ),
                None => format!(
                    "  {} stays open: closing a PR needs --force",
                    fk.format_ref(*number)
                ),
            }
        }
        Kept::Notified { number, who } => format!(
            "  {} already had the review request on {}",
            who.join(", "),
            fk.format_ref(*number)
        ),
        Kept::BaseGone { number, base } => format!(
            "  {} cannot go back to base '{base}': {} no longer has that branch",
            fk.format_ref(*number),
            entry.remote
        ),
    }
}

/// A write the command made that jjpr could not record.
pub fn missed(what: &str) -> String {
    format!("  {what}: jjpr could not read it before changing it, so it stays as it is")
}

/// The note when the command that recorded an entry stopped before finishing.
pub fn abandoned(entry: &Entry, now: u64) -> String {
    format!(
        "{} stopped before it finished; going by what it recorded.",
        name(entry, now)
    )
}

pub fn kept_heading(direction: Direction) -> &'static str {
    match direction {
        Direction::Undo => "Not undone:",
        Direction::Redo => "Not redone:",
    }
}

/// The hint after "Not undone:" when `open` PRs stayed open.
/// The last line of a dry run, which takes no snapshot of the working copy.
pub const DRY_RUN_NOTE: &str = "Nothing was changed. Edits in the working copy were not checked; the real run checks them first.";

pub fn close_hint(open: usize) -> &'static str {
    if open == 1 {
        "To close it too: jjpr undo --force"
    } else {
        "To close them too: jjpr undo --force"
    }
}

fn changed(c: &Changed, fk: ForgeKind) -> String {
    let pr = |n: &u64| fk.format_ref(*n);
    match c {
        Changed::Base { number, now } => format!("{}'s base is now '{now}'", pr(number)),
        Changed::Comment { pr: n } => format!("the stack comment on {} was edited", pr(n)),
        Changed::CommentGone { pr: n } => format!("the stack comment on {} was deleted", pr(n)),
        Changed::Body { number } => format!("the description of {} was edited", pr(number)),
        Changed::Activity { number, count } => {
            let s = if *count == 1 { "" } else { "s" };
            format!(
                "{} has {count} comment{s} or review{s} from others",
                pr(number)
            )
        }
    }
}

/// The warning `--force` prints for what it writes over.
pub fn overridden(c: &Changed, fk: ForgeKind) -> String {
    let what = changed(c, fk);
    let then = match c {
        Changed::CommentGone { .. } => "it stays deleted",
        Changed::Activity { .. } => "closing it anyway",
        _ => "restoring it anyway",
    };
    format!("  Warning: {what}; {then}.")
}

pub fn refusal(r: &Refusal, entry: &Entry, direction: Direction, now: u64) -> String {
    let fk = entry.forge;
    let lead = format!("cannot {} {}", verb(direction), name(entry, now));
    match r {
        Refusal::Merged { number } => format!(
            "{lead}: it merged {}, and a merge cannot be undone. To back the change out, \
             revert it on {fk}.",
            fk.format_ref(*number)
        ),
        Refusal::MergedSince { number } => {
            format!("{lead}: {} has been merged since.", fk.format_ref(*number))
        }
        Refusal::BranchMoved {
            bookmark,
            expected,
            now: there,
        } => {
            let remote = &entry.remote;
            let expected = expected
                .as_deref()
                .map_or("no branch".to_string(), |c| commit(c).to_string());
            let there = there
                .as_deref()
                .map_or(format!("{remote} has no such branch"), |c| {
                    format!("{remote} has {}", commit(c))
                });
            format!(
                "{lead}: '{bookmark}' changed on {remote} after jjpr last pushed it \
                 (jjpr expected {expected}; {there}). Nothing was changed."
            )
        }
        Refusal::Changed(list) => {
            let mut text = format!("{lead}: these changed on {fk} after jjpr wrote them:");
            for c in list {
                text.push_str(&format!("\n  {}", changed(c, fk)));
            }
            text.push_str(&format!(
                "\nRun `jjpr {} --force` to restore them anyway.",
                verb(direction)
            ));
            text
        }
    }
}

/// Why an entry cannot be acted on, before the forge is asked anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    WatchRunning,
    StillRunning,
    Absorbed(Vec<String>),
    NoEnd,
    OpGone(String),
    RepoChanged {
        since: Vec<Operation>,
    },
    NoRemote,
    OtherRepo,
    /// Its PRs were left open, and an older command was undone since.
    RedoFirst(String),
    /// Another jjpr command, by this pid, is changing the repo now.
    Busy(u32),
}

/// The way forward when undo refuses to lose work: keep it, and let submit
/// make the forge follow the stack (the recovering page says the same).
const KEEP_WORK: &str = "To keep that work, change the stack with jj until it is what you want, then run `jjpr submit`.";

pub fn blocked(b: &Blocked, entry: &Entry, direction: Direction, now: u64) -> String {
    let lead = format!("cannot {} {}", verb(direction), name(entry, now));
    match b {
        Blocked::WatchRunning => format!(
            "cannot {} while `jjpr watch` is running in this repo: it would redo the work. \
             Stop it, then run `jjpr {}` again.",
            verb(direction),
            verb(direction)
        ),
        Blocked::StillRunning => {
            format!("{lead}: it is still running, or stopped before it finished recording.")
        }
        Blocked::Absorbed(what) => format!(
            "{lead}: while it ran, jj also recorded work that was not jjpr's ({}), and undoing \
             would discard it.\n  {KEEP_WORK}",
            what.join(", ")
        ),
        Blocked::NoEnd => format!("{lead}: jjpr did not record where it ended."),
        Blocked::OpGone(op) => format!("{lead}: jj no longer has its operation {}.", short(op)),
        Blocked::RepoChanged { since } => {
            let mut text = format!("{lead}: the repo has changed since, and that would be lost.");
            if !since.is_empty() {
                text.push_str("\n  jj operations since:");
                for op in since.iter().take(5) {
                    text.push_str(&format!("\n    {} {}", short(&op.id), op.description));
                }
                if since.len() > 5 {
                    text.push_str(&format!("\n    and {} more", since.len() - 5));
                }
            }
            text.push_str("\n  ");
            text.push_str(KEEP_WORK);
            text
        }
        Blocked::NoRemote => format!(
            "{lead}: this repo has no remote '{}' any more.",
            entry.remote
        ),
        Blocked::OtherRepo => format!(
            "{lead}: remote '{}' no longer points at {} {}/{}.",
            entry.remote, entry.forge, entry.owner, entry.repo
        ),
        Blocked::RedoFirst(older) => format!(
            "cannot close the PRs of {} now: {older} was undone after it. Run `jjpr redo`, \
             then `jjpr undo --force`.",
            name(entry, now)
        ),
        Blocked::Busy(pid) => format!(
            "cannot {} while another jjpr command is changing this repo (pid {pid}). Wait \
             for it to finish, then run `jjpr {}` again.",
            verb(direction),
            verb(direction)
        ),
    }
}

pub fn done(entry: &Entry, direction: Direction, now: u64) -> String {
    match direction {
        Direction::Undo => format!("Undid {}. To put it back: jjpr redo", name(entry, now)),
        Direction::Redo => format!("Redid {}. To take it back: jjpr undo", name(entry, now)),
    }
}

pub fn nothing(direction: Direction) -> String {
    format!("Nothing to {}.", verb(direction))
}

/// `jjpr undo --list`.
pub fn list(entries: &[Entry], now: u64) -> Vec<String> {
    if entries.is_empty() {
        return vec!["No jjpr commands are recorded in this repo.".to_string()];
    }
    let mut lines = vec!["jjpr commands recorded in this repo, newest first:".to_string()];
    for e in entries.iter().rev() {
        let prs: Vec<String> = touched_prs(e)
            .into_iter()
            .map(|n| e.forge.format_ref(n))
            .collect();
        let note = match (e.merged(), e.state) {
            (Some(n), _) => format!("merged {}: cannot be undone", e.forge.format_ref(n)),
            (None, State::Done) => String::new(),
            (None, State::Undone) => "undone".to_string(),
            (None, State::PartlyUndone) => "partly undone".to_string(),
            (None, State::KeptOpen) => "undone, its PRs still open".to_string(),
            (None, State::Running) => "running, or stopped early".to_string(),
        };
        let line = format!(
            "  {:<16} {:<7} {:<20} {note}",
            when(e.started_at, now),
            e.command,
            prs.join(" ")
        );
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// "14:02" today, "2026-10-05 14:02" otherwise, in local time.
pub fn when(secs: u64, now: u64) -> String {
    let (date, time) = crate::clock::local(secs);
    if date == crate::clock::local(now).0 {
        time
    } else {
        format!("{date} {time}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::undo::journal::{Record, SCHEMA};

    fn entry() -> Entry {
        Entry {
            schema: SCHEMA,
            id: "1".into(),
            command: "submit".into(),
            started_at: 1_700_000_000,
            remote: "origin".into(),
            forge: ForgeKind::GitHub,
            owner: "o".into(),
            repo: "r".into(),
            start_op: "4f2a9c1e0b7d6a5c".into(),
            end_op: Some("e".into()),
            end_view: None,
            absorbed: vec![],
            state: State::Done,
            local_undone: false,
            undone_view: None,
            last_op: None,
            closed_by_push: Vec::new(),
            missed: Vec::new(),
            actions: vec![Record {
                action: Action::CreatePr {
                    number: 44,
                    head: "settings".into(),
                },
                confirmed: true,
                undone: false,
            }],
        }
    }

    fn undo(step: Step) -> String {
        super::step(&step, &entry(), Direction::Undo, ForgeKind::GitHub)
    }

    fn redo(step: Step) -> String {
        super::step(&step, &entry(), Direction::Redo, ForgeKind::GitLab)
    }

    fn push(from: Option<&str>, to: Option<&str>) -> Step {
        Step::Push {
            record: 0,
            bookmark: "auth".into(),
            remote: "origin".into(),
            from: from.map(Into::into),
            to: to.map(Into::into),
        }
    }

    #[test]
    fn every_step_reads_as_one_line() {
        let local = Step::Local {
            op: "4f2a9c1e0b7d6a5c".into(),
        };
        assert_eq!(
            undo(local.clone()),
            "  Restore the local repo to operation 4f2a9c1e0b7d, from before the submit"
        );
        assert_eq!(
            redo(local),
            "  Restore the local repo to operation 4f2a9c1e0b7d, from after the submit"
        );
        let (a, b) = (Some("9e8f7a6b5c4d"), Some("1a2b3c4d5e6f"));
        assert_eq!(undo(push(a, b)), "  Force-push 'auth' back to 1a2b3c4d");
        assert_eq!(redo(push(b, a)), "  Force-push 'auth' to 9e8f7a6b again");
        assert_eq!(undo(push(a, None)), "  Delete branch 'auth' from origin");
        assert_eq!(redo(push(None, a)), "  Push 'auth' to origin again");
        let reopen = Step::Reopen {
            record: 0,
            number: 5,
        };
        assert_eq!(undo(reopen.clone()), "  Reopen #5, which the push closed");
        assert_eq!(redo(reopen), "  Reopen !5");
        let close = Step::Close {
            record: 0,
            number: 44,
        };
        assert_eq!(undo(close), "  Close #44, which the submit opened");
        let base = Step::Base {
            record: 0,
            number: 42,
            from: "main".into(),
            to: "auth".into(),
        };
        assert_eq!(
            undo(base.clone()),
            "  Retarget #42 from 'main' back to 'auth'"
        );
        assert_eq!(redo(base), "  Retarget !42 from 'main' to 'auth' again");
        let delete = Step::DeleteComment {
            record: 0,
            pr: 1,
            id: 2,
        };
        assert_eq!(undo(delete), "  Delete the stack comment on #1");
        let edit = Step::EditComment {
            record: 0,
            pr: 1,
            id: 2,
            to: String::new(),
        };
        assert_eq!(undo(edit.clone()), "  Restore the stack comment on #1");
        assert_eq!(redo(edit), "  Rewrite the stack comment on !1");
        let post = Step::PostComment {
            record: 0,
            pr: 1,
            id: 2,
            body: String::new(),
        };
        assert_eq!(undo(post.clone()), "  Put back the stack comment on #1");
        assert_eq!(redo(post), "  Post the stack comment on !1 again");
        let body = Step::Body {
            record: 0,
            number: 3,
            to: String::new(),
        };
        assert_eq!(undo(body.clone()), "  Restore the description of #3");
        assert_eq!(redo(body), "  Rewrite the description of !3");
        let draft = Step::Draft {
            record: 0,
            number: 3,
        };
        assert_eq!(undo(draft), "  Mark #3 as a draft again");
        let ready = Step::Ready {
            record: 0,
            number: 3,
        };
        assert_eq!(redo(ready), "  Mark !3 as ready for review again");
        let who = vec!["alice".to_string(), "bob".to_string()];
        let unrequest = Step::Unrequest {
            record: 0,
            number: 3,
            who: who.clone(),
        };
        assert_eq!(
            undo(unrequest),
            "  Withdraw the review request to alice, bob on #3"
        );
        let request = Step::Request {
            record: 0,
            number: 3,
            who,
        };
        assert_eq!(
            redo(request),
            "  Request a review from alice, bob on !3 again"
        );
    }

    #[test]
    fn kept_lines_say_what_stays_and_why() {
        let mut e = entry();
        let fk = ForgeKind::GitHub;
        assert_eq!(
            kept(&Kept::OpenPr { number: 44 }, &e, fk),
            "  #44 stays open: closing a PR needs --force",
            "a branch that was already on the remote is not the command's to delete"
        );
        e.actions.insert(
            0,
            Record {
                action: Action::Push {
                    bookmark: "settings".into(),
                    remote: "origin".into(),
                    before: None,
                    after: "c".into(),
                    pr: None,
                },
                confirmed: true,
                undone: false,
            },
        );
        assert_eq!(
            kept(&Kept::OpenPr { number: 44 }, &e, fk),
            "  #44 and its branch 'settings' stay open: closing a PR needs --force"
        );
        assert_eq!(close_hint(1), "To close it too: jjpr undo --force");
        assert_eq!(close_hint(2), "To close them too: jjpr undo --force");
        assert_eq!(
            kept(
                &Kept::Notified {
                    number: 3,
                    who: vec!["alice".into()]
                },
                &e,
                fk
            ),
            "  alice already had the review request on #3"
        );
        assert_eq!(
            kept(
                &Kept::BaseGone {
                    number: 2,
                    base: "bottom".into()
                },
                &e,
                fk
            ),
            "  #2 cannot go back to base 'bottom': origin no longer has that branch"
        );
        assert_eq!(kept_heading(Direction::Redo), "Not redone:");
    }

    #[test]
    fn force_names_what_it_writes_over() {
        let fk = ForgeKind::GitHub;
        let base = Changed::Base {
            number: 2,
            now: "dev".into(),
        };
        assert_eq!(
            overridden(&base, fk),
            "  Warning: #2's base is now 'dev'; restoring it anyway."
        );
        assert_eq!(
            overridden(&Changed::CommentGone { pr: 1 }, fk),
            "  Warning: the stack comment on #1 was deleted; it stays deleted."
        );
        assert_eq!(
            overridden(
                &Changed::Activity {
                    number: 4,
                    count: 1
                },
                fk
            ),
            "  Warning: #4 has 1 comment or review from others; closing it anyway."
        );
        assert_eq!(
            overridden(
                &Changed::Activity {
                    number: 4,
                    count: 2
                },
                fk
            ),
            "  Warning: #4 has 2 comments or reviews from others; closing it anyway."
        );
    }

    #[test]
    fn refusals_name_the_command_and_the_way_out() {
        let e = entry();
        let now = e.started_at;
        let at = when(e.started_at, now);
        let text = refusal(&Refusal::Merged { number: 41 }, &e, Direction::Undo, now);
        assert_eq!(
            text,
            format!(
                "cannot undo `jjpr submit` from {at}: it merged #41, and a merge cannot be \
                 undone. To back the change out, revert it on GitHub."
            )
        );
        let moved = Refusal::BranchMoved {
            bookmark: "auth".into(),
            expected: Some("9e8f7a6b5c".into()),
            now: Some("3c4d5e6f7a".into()),
        };
        assert_eq!(
            refusal(&moved, &e, Direction::Undo, now),
            format!(
                "cannot undo `jjpr submit` from {at}: 'auth' changed on origin after jjpr last \
                 pushed it (jjpr expected 9e8f7a6b; origin has 3c4d5e6f). Nothing was changed."
            )
        );
        let changed = Refusal::Changed(vec![Changed::Body { number: 3 }]);
        assert_eq!(
            refusal(&changed, &e, Direction::Redo, now),
            format!(
                "cannot redo `jjpr submit` from {at}: these changed on GitHub after jjpr wrote \
                 them:\n  the description of #3 was edited\nRun `jjpr redo --force` to restore \
                 them anyway."
            )
        );
    }

    #[test]
    fn blocked_names_the_cause() {
        let e = entry();
        let now = e.started_at;
        let at = when(now, now);
        let since = vec![Operation {
            id: "8c1d2e3f4a5b6c7d".into(),
            description: "describe commit 5d6e".into(),
        }];
        let changed = Blocked::RepoChanged { since };
        assert_eq!(
            blocked(&changed, &e, Direction::Undo, now),
            format!(
                "cannot undo `jjpr submit` from {at}: the repo has changed since, and that would \
                 be lost.\n  jj operations since:\n    8c1d2e3f4a5b describe commit 5d6e\n  \
                 To keep that work, change the stack with jj until it is what you want, then \
                 run `jjpr submit`."
            )
        );
        let absorbed = Blocked::Absorbed(vec!["snapshot working copy".into()]);
        assert_eq!(
            blocked(&absorbed, &e, Direction::Undo, now),
            format!(
                "cannot undo `jjpr submit` from {at}: while it ran, jj also recorded work that \
                 was not jjpr's (snapshot working copy), and undoing would discard it.\n  \
                 To keep that work, change the stack with jj until it is what you want, then \
                 run `jjpr submit`."
            )
        );
        assert_eq!(
            blocked(&Blocked::WatchRunning, &e, Direction::Redo, now),
            "cannot redo while `jjpr watch` is running in this repo: it would redo the work. \
             Stop it, then run `jjpr redo` again."
        );
    }

    #[test]
    fn the_time_is_shown_with_a_date_unless_it_is_today() {
        let t = 1_700_000_000;
        assert_eq!(when(t, t).len(), 5);
        assert!(when(t, t + 3 * 86_400).starts_with("2023-11-"));
    }

    #[test]
    fn done_says_how_to_go_back() {
        let e = entry();
        let at = when(e.started_at, e.started_at);
        assert_eq!(
            done(&e, Direction::Undo, e.started_at),
            format!("Undid `jjpr submit` from {at}. To put it back: jjpr redo")
        );
        assert_eq!(
            done(&e, Direction::Redo, e.started_at),
            format!("Redid `jjpr submit` from {at}. To take it back: jjpr undo")
        );
    }
}
