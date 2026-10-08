//! Every line `jjpr undo` and `jjpr redo` print, in one place.

use crate::forge::ForgeKind;

use super::journal::{Entry, State};
use super::plan::{Direction, Kept, Step, touched_prs};

/// "`jjpr submit` from 14:02".
pub fn name(entry: &Entry, now: u64) -> String {
    let at = when(entry.started_at, now);
    if entry.command == super::step_back::COMMAND {
        return format!("the jj operations stepped back over at {at}");
    }
    format!("`jjpr {}` from {at}", entry.command)
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

pub(super) fn short(id: &str) -> &str {
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
            let what = if cmd == super::step_back::COMMAND {
                "jj operations"
            } else {
                cmd.as_str()
            };
            format!(
                "Restore the local repo to operation {}, from {side} the {what}",
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
pub fn kept(kept: &Kept, fk: ForgeKind) -> String {
    match kept {
        Kept::Notified { number, who } => format!(
            "  {} already had the review request on {}",
            who.join(", "),
            fk.format_ref(*number)
        ),
        Kept::ApprovalsDismissed { number, count } => {
            let s = if *count == 1 { "" } else { "s" };
            format!(
                "  {}: pushing its branch dismisses its {count} approval{s}",
                fk.format_ref(*number)
            )
        }
        Kept::Activity { number, count } => {
            let s = if *count == 1 { "" } else { "s" };
            format!(
                "  {} has {count} comment{s} or review{s} from others; they show again once it reopens",
                fk.format_ref(*number)
            )
        }
        Kept::RequestedAgain { number, who } => format!(
            "  {} {} the review request on {} again",
            who.join(", "),
            if who.len() == 1 { "gets" } else { "get" },
            fk.format_ref(*number)
        ),
    }
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
        Direction::Redo => "Worth knowing:",
    }
}

/// The last line of a dry run, which takes no snapshot of the working copy.
pub const DRY_RUN_NOTE: &str = "Nothing was changed. Edits in the working copy were not checked; the real run checks them first.";

/// Why an entry cannot be acted on at all, before the forge is asked anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    WatchRunning,
    StillRunning,
    NoEnd,
    OpGone(String),
    NoRemote,
    OtherRepo,
    /// Another jjpr command, by this pid, is changing the repo now.
    Busy(u32),
}

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
        Blocked::NoEnd => format!("{lead}: jjpr did not record where it ended."),
        Blocked::OpGone(op) => format!("{lead}: jj no longer has its operation {}.", short(op)),
        Blocked::NoRemote => format!(
            "{lead}: this repo has no remote '{}' any more.",
            entry.remote
        ),
        Blocked::OtherRepo => format!(
            "{lead}: remote '{}' no longer points at {} {}/{}.",
            entry.remote, entry.forge, entry.owner, entry.repo
        ),
        Blocked::Busy(pid) => format!(
            "cannot {} while another jjpr command is changing this repo (pid {pid}). Wait \
             for it to finish, then run `jjpr {}` again.",
            verb(direction),
            verb(direction)
        ),
    }
}

/// The line a command ends with once it has recorded something undo can take
/// back. Only submit: a merge cannot be undone, and watch runs on.
pub fn after_command(command: &str) -> Option<&'static str> {
    (command == "submit").then_some("To take it back: jjpr undo")
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
    use crate::undo::journal::{Action, Record, SCHEMA};

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
            body: String::new(),
        };
        assert_eq!(undo(delete), "  Delete the stack comment on #1");
        let edit = Step::EditComment {
            record: 0,
            pr: 1,
            id: 2,
            from: String::new(),
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
            from: String::new(),
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
        assert_eq!(
            kept(
                &Kept::Notified {
                    number: 3,
                    who: vec!["alice".into()]
                },
                ForgeKind::GitHub
            ),
            "  alice already had the review request on #3"
        );
        assert_eq!(kept_heading(Direction::Undo), "Not undone:");
        assert_eq!(kept_heading(Direction::Redo), "Worth knowing:");
    }

    #[test]
    fn blocked_names_the_cause() {
        let e = entry();
        let now = e.started_at;
        let at = when(now, now);
        assert_eq!(
            blocked(&Blocked::WatchRunning, &e, Direction::Redo, now),
            "cannot redo while `jjpr watch` is running in this repo: it would redo the work. \
             Stop it, then run `jjpr redo` again."
        );
        assert_eq!(
            blocked(
                &Blocked::OpGone("8c1d2e3f4a5b6c7d".into()),
                &e,
                Direction::Undo,
                now
            ),
            format!(
                "cannot undo `jjpr submit` from {at}: jj no longer has its operation 8c1d2e3f4a5b."
            )
        );
        assert_eq!(
            blocked(&Blocked::Busy(7), &e, Direction::Undo, now),
            "cannot undo while another jjpr command is changing this repo (pid 7). Wait for it \
             to finish, then run `jjpr undo` again."
        );
    }

    #[test]
    fn the_time_is_shown_with_a_date_unless_it_is_today() {
        let t = 1_700_000_000;
        assert_eq!(when(t, t).len(), 5);
        assert!(when(t, t + 3 * 86_400).starts_with("2023-11-"));
    }

    #[test]
    fn only_a_submit_ends_with_the_undo_hint() {
        assert_eq!(after_command("submit"), Some("To take it back: jjpr undo"));
        assert_eq!(after_command("merge"), None);
        assert_eq!(after_command("watch"), None);
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
