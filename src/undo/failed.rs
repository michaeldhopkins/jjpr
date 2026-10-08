//! What `jjpr undo` and `jjpr redo` say when a run fails at a step: the
//! steps it puts back, and how far that got.

use crate::forge::ForgeKind;

use super::journal::Entry;
use super::plan::{Direction, Status, Step};
use super::report::{name, short};
use super::rollback::Difference;

fn verb(direction: Direction) -> &'static str {
    match direction {
        Direction::Undo => "undo",
        Direction::Redo => "redo",
    }
}

fn commit(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A step being put back after a failure, in words that do not depend on
/// which way the run was going.
pub fn back_step(step: &Step, fk: ForgeKind) -> String {
    let pr = |n: &u64| fk.format_ref(*n);
    let text = match step {
        Step::Local { op } => format!("Restore the local repo to operation {}", short(op)),
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
            ..
        } => format!("Push '{bookmark}' to {remote} again"),
        Step::Push {
            bookmark,
            to: Some(to),
            ..
        } => format!("Force-push '{bookmark}' back to {}", commit(to)),
        Step::Reopen { number, .. } => format!("Reopen {}", pr(number)),
        Step::Close { number, .. } => format!("Close {}", pr(number)),
        Step::Base { number, to, .. } => format!("Retarget {} back to '{to}'", pr(number)),
        Step::DeleteComment { pr: n, .. } => format!("Delete the stack comment on {}", pr(n)),
        Step::EditComment { pr: n, .. } => format!("Restore the stack comment on {}", pr(n)),
        Step::PostComment { pr: n, .. } => format!("Put back the stack comment on {}", pr(n)),
        Step::Body { number, .. } => format!("Restore the description of {}", pr(number)),
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

/// The heading before the steps a failed run puts back.
pub fn putting_back(direction: Direction) -> String {
    format!(
        "That step failed. Putting back what this {} changed:",
        verb(direction)
    )
}

/// A run failed at a step and put back everything it had done. `left`:
/// what still differs from before the run, by a check made afterwards.
pub fn stopped_and_put_back(
    entry: &Entry,
    direction: Direction,
    now: u64,
    cause: &str,
    left: &[Difference],
) -> String {
    let mut text = format!(
        "{} of {} failed: {cause}",
        title(direction),
        name(entry, now)
    );
    if left.is_empty() {
        text.push_str(&format!(
            "\njjpr put back everything it had changed, so nothing is changed. Fix the \
             problem, then run `jjpr {}` again.",
            verb(direction)
        ));
        return text;
    }
    text.push_str("\njjpr put back what it had changed, but these are not as they were:");
    for d in left {
        text.push_str(&format!("\n  - {}", difference(d, entry)));
    }
    text.push_str(&format!(
        "\nThe step that failed may have gone through after all. `jjpr {} --dry-run` \
         shows where that leaves it.",
        verb(direction)
    ));
    text
}

/// A run failed at a step, and putting back what it had done failed too.
pub fn stopped_partway(
    entry: &Entry,
    direction: Direction,
    now: u64,
    cause: &str,
    put_back: &str,
) -> String {
    let (done, finish, back) = match direction {
        Direction::Undo => (
            "partly undone",
            "finish undoing it",
            "jjpr redo` to put back",
        ),
        Direction::Redo => (
            "partly redone",
            "finish redoing it",
            "jjpr undo` to take back",
        ),
    };
    format!(
        "{} of {} failed: {cause}\nPutting back what it had changed failed too: {put_back}\n\
         It is {done}. Fix the problem, then run `jjpr {}` to {finish}, or `{back} what it \
         did. Each checks everything before it changes anything.",
        title(direction),
        name(entry, now),
        verb(direction)
    )
}

fn title(direction: Direction) -> &'static str {
    match direction {
        Direction::Undo => "The undo",
        Direction::Redo => "The redo",
    }
}

fn status(s: Status) -> &'static str {
    match s {
        Status::Open => "open",
        Status::Closed => "closed",
        Status::Merged => "merged",
    }
}

fn difference(d: &Difference, entry: &Entry) -> String {
    let fk = entry.forge;
    let pr = |n: &u64| fk.format_ref(*n);
    let at = |c: &Option<String>| match c {
        Some(c) => format!("at {}", commit(c)),
        None => "gone".to_string(),
    };
    match d {
        Difference::Local => "the local repo".to_string(),
        Difference::Branch { name, was, now } => format!(
            "'{name}' on {} is {} (it was {})",
            entry.remote,
            at(now),
            at(was)
        ),
        Difference::Status { number, was, now } => format!(
            "{} is {} (it was {})",
            pr(number),
            status(*now),
            status(*was)
        ),
        Difference::Base { number, was, now } => {
            format!("{} targets '{now}' (it targeted '{was}')", pr(number))
        }
        Difference::Body { number } => format!("the description of {}", pr(number)),
        Difference::Draft { number, now } => {
            let s = if *now { "a draft" } else { "ready for review" };
            format!("{} is {s}", pr(number))
        }
        Difference::Reviewers { number } => format!("the review requests on {}", pr(number)),
        Difference::Comments { pr: n } => format!("the stack comments on {}", pr(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::undo::journal::{SCHEMA, State};
    use crate::undo::report::when;

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
            start_op: "s".into(),
            end_op: Some("e".into()),
            end_view: None,
            absorbed: vec![],
            state: State::Done,
            local_undone: false,
            undone_view: None,
            last_op: None,
            closed_by_push: Vec::new(),
            missed: Vec::new(),
            actions: vec![],
        }
    }

    fn at() -> String {
        when(1_700_000_000, 1_700_000_000)
    }

    #[test]
    fn steps_put_back_read_the_same_either_way() {
        let fk = ForgeKind::GitLab;
        let push = |from: Option<&str>, to: Option<&str>| Step::Push {
            record: 0,
            bookmark: "auth".into(),
            remote: "origin".into(),
            from: from.map(Into::into),
            to: to.map(Into::into),
        };
        let lines: Vec<String> = [
            Step::Local {
                op: "4f2a9c1e0b7d6a5c".into(),
            },
            push(Some("c1"), None),
            push(None, Some("c1")),
            push(Some("c1"), Some("9e8f7a6b5c")),
            Step::Reopen {
                record: 0,
                number: 5,
            },
            Step::Close {
                record: 0,
                number: 5,
            },
            Step::Base {
                record: 0,
                number: 5,
                from: "a".into(),
                to: "main".into(),
            },
            Step::DeleteComment {
                record: 0,
                pr: 5,
                id: 1,
                body: String::new(),
            },
            Step::EditComment {
                record: 0,
                pr: 5,
                id: 1,
                from: String::new(),
                to: String::new(),
            },
            Step::PostComment {
                record: 0,
                pr: 5,
                id: 1,
                body: String::new(),
            },
            Step::Body {
                record: 0,
                number: 5,
                from: String::new(),
                to: String::new(),
            },
            Step::Draft {
                record: 0,
                number: 5,
            },
            Step::Ready {
                record: 0,
                number: 5,
            },
            Step::Unrequest {
                record: 0,
                number: 5,
                who: vec!["alice".into()],
            },
            Step::Request {
                record: 0,
                number: 5,
                who: vec!["alice".into()],
            },
        ]
        .iter()
        .map(|s| back_step(s, fk))
        .collect();
        assert_eq!(
            lines,
            [
                "  Restore the local repo to operation 4f2a9c1e0b7d",
                "  Delete branch 'auth' from origin",
                "  Push 'auth' to origin again",
                "  Force-push 'auth' back to 9e8f7a6b",
                "  Reopen !5",
                "  Close !5",
                "  Retarget !5 back to 'main'",
                "  Delete the stack comment on !5",
                "  Restore the stack comment on !5",
                "  Put back the stack comment on !5",
                "  Restore the description of !5",
                "  Mark !5 as a draft again",
                "  Mark !5 as ready for review again",
                "  Withdraw the review request to alice on !5",
                "  Request a review from alice on !5 again",
            ]
        );
        assert_eq!(
            putting_back(Direction::Redo),
            "That step failed. Putting back what this redo changed:"
        );
    }

    #[test]
    fn a_failed_run_put_back_says_nothing_changed() {
        let e = entry();
        let text = stopped_and_put_back(&e, Direction::Undo, e.started_at, "HTTP 502", &[]);
        assert_eq!(
            text,
            format!(
                "The undo of `jjpr submit` from {} failed: HTTP 502\njjpr put back everything \
                 it had changed, so nothing is changed. Fix the problem, then run `jjpr undo` \
                 again.",
                at()
            )
        );
    }

    #[test]
    fn a_failed_run_names_what_the_check_afterwards_found() {
        let e = entry();
        let left = [
            Difference::Local,
            Difference::Branch {
                name: "auth".into(),
                was: Some("9e8f7a6b5c".into()),
                now: None,
            },
            Difference::Status {
                number: 44,
                was: Status::Open,
                now: Status::Closed,
            },
            Difference::Base {
                number: 44,
                was: "auth".into(),
                now: "main".into(),
            },
            Difference::Body { number: 44 },
            Difference::Draft {
                number: 44,
                now: true,
            },
            Difference::Draft {
                number: 44,
                now: false,
            },
            Difference::Reviewers { number: 44 },
            Difference::Comments { pr: 44 },
        ];
        let text = stopped_and_put_back(&e, Direction::Redo, e.started_at, "HTTP 502", &left);
        assert_eq!(
            text,
            format!(
                "The redo of `jjpr submit` from {} failed: HTTP 502\njjpr put back what it had \
                 changed, but these are not as they were:\n  - the local repo\n  - 'auth' on \
                 origin is gone (it was at 9e8f7a6b)\n  - #44 is closed (it was open)\n  - #44 \
                 targets 'main' (it targeted 'auth')\n  - the description of #44\n  - #44 is a \
                 draft\n  - #44 is ready for review\n  - the review requests on #44\n  - the \
                 stack comments on #44\nThe step that failed may have gone through after all. \
                 `jjpr redo --dry-run` shows where that leaves it.",
                at()
            )
        );
    }

    #[test]
    fn a_failed_put_back_says_both_ways_forward() {
        let e = entry();
        let text = stopped_partway(&e, Direction::Undo, e.started_at, "HTTP 502", "HTTP 503");
        assert_eq!(
            text,
            format!(
                "The undo of `jjpr submit` from {} failed: HTTP 502\nPutting back what it had \
                 changed failed too: HTTP 503\nIt is partly undone. Fix the problem, then run \
                 `jjpr undo` to finish undoing it, or `jjpr redo` to put back what it did. Each \
                 checks everything before it changes anything.",
                at()
            )
        );
        let text = stopped_partway(&e, Direction::Redo, e.started_at, "x", "y");
        assert!(text.contains(
            "It is partly redone. Fix the problem, then run `jjpr redo` to finish redoing it, \
             or `jjpr undo` to take back what it did."
        ));
    }
}
