//! What `jjpr undo` and `jjpr redo` say when they cannot do the whole job:
//! the blockers that stop a run before it changes anything, and what
//! `--force` writes over. A run that fails partway is [`super::failed`]'s.

use crate::forge::ForgeKind;

use super::journal::Entry;
use super::plan::{Blocker, Changed, Direction};
use super::repo::Operation;
use super::report::{name, short};

fn verb(direction: Direction) -> &'static str {
    match direction {
        Direction::Undo => "undo",
        Direction::Redo => "redo",
    }
}

fn commit(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Why `b` stops the run, as one line (a few for the repo's operations).
pub fn reason(b: &Blocker, entry: &Entry) -> String {
    let fk = entry.forge;
    let pr = |n: &u64| fk.format_ref(*n);
    let remote = &entry.remote;
    match b {
        Blocker::Changed(c) => changed(c, fk),
        Blocker::Close { number, activity } => {
            let opened = format!(
                "{}, which the {} opened, would be closed",
                pr(number),
                entry.command
            );
            if *activity == 0 {
                opened
            } else {
                format!("{opened}, and it has {}", activity_text(*activity))
            }
        }
        Blocker::BaseGone { number, base } => format!(
            "{} can't go back to base '{base}': {remote} no longer has that branch",
            pr(number)
        ),
        Blocker::Merged { number } => format!(
            "it merged {}, and a merge can't be undone. To back the change out, revert it on {fk}",
            pr(number)
        ),
        Blocker::MergedSince { number } => format!("{} has been merged since", pr(number)),
        Blocker::BranchMoved {
            bookmark,
            expected,
            now,
        } => {
            let expected = expected
                .as_deref()
                .map_or("no branch".to_string(), |c| commit(c).to_string());
            let there = now
                .as_deref()
                .map_or(format!("{remote} has no such branch"), |c| {
                    format!("{remote} has {}", commit(c))
                });
            format!(
                "'{bookmark}' changed on {remote} after jjpr last pushed it \
                 (jjpr expected {expected}; {there})"
            )
        }
        Blocker::WontReopen { number } => format!(
            "the push closed {}, and {fk} won't reopen a PR whose branch moved while it was closed",
            pr(number)
        ),
        Blocker::Missed(what) => {
            format!("{what}: jjpr couldn't read it before changing it, so it can't put it back")
        }
        Blocker::RepoChanged { since } => {
            let mut text = "the repo changed since, and that work would be lost".to_string();
            if !since.is_empty() {
                text.push_str("\n    jj operations since:");
                for op in since.iter().take(5) {
                    text.push_str(&format!("\n      {} {}", short(&op.id), op.description));
                }
                if since.len() > 5 {
                    text.push_str(&format!("\n      and {} more", since.len() - 5));
                }
            }
            text
        }
        Blocker::EditsOnDisk { files } => format!(
            "undoing it first takes back the jj work since, and that takes your edits to {} off \
             the disk (`jjpr redo` brings them back)",
            listed(files)
        ),
        Blocker::NotLocal { what } => {
            format!("{what} since, so jjpr can't take back the jj work since on its own")
        }
        Blocker::ReopenBaseGone { number, base } => format!(
            "{} can't be reopened: {remote} no longer has its base branch '{base}'",
            pr(number)
        ),
        Blocker::Absorbed(what) => format!(
            "while it ran, jj also recorded work that wasn't jjpr's ({}), and that work \
             would be lost",
            what.join(", ")
        ),
    }
}

/// Up to five names, then how many more.
fn listed(names: &[String]) -> String {
    let mut text = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > 5 {
        text.push_str(&format!(" and {} more", names.len() - 5));
    }
    text
}

/// What `jjpr undo` says when it steps back over the jj work since `name`,
/// or in a dry run would.
pub fn stepping_back(since: &[Operation], name: &str, dry_run: bool) -> String {
    let n = since.len();
    let ops = if n == 1 {
        "1 jj operation".to_string()
    } else {
        format!("{n} jj operations")
    };
    let mut text = if dry_run {
        format!("Would undo {ops} since {name}, so that the next `jjpr undo` reaches it:")
    } else {
        format!("Undid {ops} since {name}:")
    };
    for op in since.iter().take(5) {
        text.push_str(&format!("\n  {} {}", short(&op.id), op.description));
    }
    if n > 5 {
        text.push_str(&format!("\n  and {} more", n - 5));
    }
    if !dry_run {
        let them = if n == 1 { "it" } else { "them" };
        text.push_str(&format!(
            "\nTo put {them} back: jjpr redo. To undo {name}: jjpr undo"
        ));
    }
    text
}

fn activity_text(count: usize) -> String {
    if count == 1 {
        "1 comment or review from others".to_string()
    } else {
        format!("{count} comments or reviews from others")
    }
}

fn changed(c: &Changed, fk: ForgeKind) -> String {
    let pr = |n: &u64| fk.format_ref(*n);
    match c {
        Changed::Base { number, now } => {
            format!(
                "{}'s base was changed to '{now}' after jjpr set it",
                pr(number)
            )
        }
        Changed::Comment { pr: n } => {
            format!(
                "the stack comment on {} was edited after jjpr wrote it",
                pr(n)
            )
        }
        Changed::CommentGone { pr: n } => {
            format!(
                "the stack comment on {} was deleted after jjpr wrote it",
                pr(n)
            )
        }
        Changed::Body { number } => {
            format!(
                "the description of {} was edited after jjpr wrote it",
                pr(number)
            )
        }
    }
}

/// The blockers a run stops on, `force` given: none when it may go ahead.
/// When any is beyond `--force`, only those are named.
fn stopping(blockers: &[Blocker], force: bool) -> Vec<&Blocker> {
    let hard: Vec<&Blocker> = blockers.iter().filter(|b| !b.forceable()).collect();
    if !hard.is_empty() {
        return hard;
    }
    if force {
        return Vec::new();
    }
    blockers.iter().collect()
}

/// The way out, after the list of reasons.
fn way_out(stops: &[&Blocker], direction: Direction, remote: &str) -> String {
    if stops.iter().all(|b| b.forceable()) {
        return format!(
            "Nothing else stands in the way. Rerun with --force to {} all of it: jjpr {} --force",
            verb(direction),
            verb(direction)
        );
    }
    let mut text =
        "Use jj to get the stack into the state you want, then run `jjpr submit`.".to_string();
    // Someone else's commits on a branch: the way to keep them.
    for b in stops {
        if let Blocker::BranchMoved { bookmark, .. } = b {
            text.push_str(&format!(
                "\nTo keep the commits on '{bookmark}': run `jj git fetch`, rebase onto \
                 `{bookmark}@{remote}`, then run `jjpr submit`."
            ));
        }
    }
    text
}

/// "jjpr can't undo all of `jjpr submit` from 14:02 without --force".
fn lead(stops: &[&Blocker], entry: &Entry, direction: Direction, now: u64) -> String {
    let without = if stops.iter().all(|b| b.forceable()) {
        " without --force"
    } else {
        ""
    };
    format!(
        "jjpr can't {} all of {}{without}",
        verb(direction),
        name(entry, now)
    )
}

fn reasons(blockers: &[&Blocker], entry: &Entry) -> String {
    blockers
        .iter()
        .map(|b| format!("\n  - {}", reason(b, entry)))
        .collect()
}

/// The error a real run stops with, having changed nothing; `None` when the
/// blockers leave it free to go ahead.
pub fn refused(
    blockers: &[Blocker],
    entry: &Entry,
    direction: Direction,
    force: bool,
    now: u64,
) -> Option<String> {
    let stops = stopping(blockers, force);
    if stops.is_empty() {
        return None;
    }
    Some(format!(
        "{}, so it changed nothing:{}\n{}",
        lead(&stops, entry, direction, now),
        reasons(&stops, entry),
        way_out(&stops, direction, &entry.remote)
    ))
}

/// The dry run's account of what would stop the real run, naming every
/// blocker and not only those that decide it; `None` when nothing would.
pub fn dry_run_blockers(
    blockers: &[Blocker],
    entry: &Entry,
    direction: Direction,
    force: bool,
    now: u64,
) -> Option<String> {
    let stops = stopping(blockers, force);
    if stops.is_empty() {
        return None;
    }
    let all: Vec<&Blocker> = blockers.iter().collect();
    Some(format!(
        "{}, so the real run would change nothing:{}\n{}",
        lead(&stops, entry, direction, now),
        reasons(&all, entry),
        way_out(&stops, direction, &entry.remote)
    ))
}

/// The warning `--force` prints for a blocker it goes past; `None` for one
/// that needs no warning (closing a PR nobody else touched).
pub fn overridden(b: &Blocker, entry: &Entry) -> Option<String> {
    let fk = entry.forge;
    let text = match b {
        Blocker::Changed(c @ Changed::CommentGone { .. }) => {
            format!("{}; posting it again", changed(c, fk))
        }
        Blocker::Changed(c) => format!("{}; restoring it anyway", changed(c, fk)),
        Blocker::Close { number, activity } if *activity > 0 => format!(
            "{} has {}; closing it anyway",
            fk.format_ref(*number),
            activity_text(*activity)
        ),
        Blocker::BaseGone { .. } => format!("{}; leaving its base as it is", reason(b, entry)),
        _ => return None,
    };
    Some(format!("  Warning: {text}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::undo::journal::{SCHEMA, State};
    use crate::undo::repo::Operation;
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

    fn close(number: u64, activity: usize) -> Blocker {
        Blocker::Close { number, activity }
    }

    fn moved() -> Blocker {
        Blocker::BranchMoved {
            bookmark: "auth".into(),
            expected: Some("9e8f7a6b5c".into()),
            now: Some("3c4d5e6f7a".into()),
        }
    }

    #[test]
    fn every_blocker_reads_as_one_reason() {
        let e = entry();
        let r = |b: Blocker| reason(&b, &e);
        assert_eq!(
            r(close(44, 0)),
            "#44, which the submit opened, would be closed"
        );
        assert_eq!(
            r(close(44, 1)),
            "#44, which the submit opened, would be closed, and it has 1 comment or review \
             from others"
        );
        assert_eq!(
            r(close(44, 3)),
            "#44, which the submit opened, would be closed, and it has 3 comments or reviews \
             from others"
        );
        assert_eq!(
            r(Blocker::Changed(Changed::Base {
                number: 2,
                now: "dev".into()
            })),
            "#2's base was changed to 'dev' after jjpr set it"
        );
        assert_eq!(
            r(Blocker::Changed(Changed::Comment { pr: 1 })),
            "the stack comment on #1 was edited after jjpr wrote it"
        );
        assert_eq!(
            r(Blocker::Changed(Changed::CommentGone { pr: 1 })),
            "the stack comment on #1 was deleted after jjpr wrote it"
        );
        assert_eq!(
            r(Blocker::Changed(Changed::Body { number: 3 })),
            "the description of #3 was edited after jjpr wrote it"
        );
        assert_eq!(
            r(Blocker::BaseGone {
                number: 2,
                base: "bottom".into()
            }),
            "#2 can't go back to base 'bottom': origin no longer has that branch"
        );
        assert_eq!(
            r(Blocker::Merged { number: 41 }),
            "it merged #41, and a merge can't be undone. To back the change out, revert it on \
             GitHub"
        );
        assert_eq!(
            r(Blocker::MergedSince { number: 41 }),
            "#41 has been merged since"
        );
        assert_eq!(
            r(moved()),
            "'auth' changed on origin after jjpr last pushed it (jjpr expected 9e8f7a6b; \
             origin has 3c4d5e6f)"
        );
        assert_eq!(
            r(Blocker::BranchMoved {
                bookmark: "auth".into(),
                expected: None,
                now: None,
            }),
            "'auth' changed on origin after jjpr last pushed it (jjpr expected no branch; \
             origin has no such branch)"
        );
        assert_eq!(
            r(Blocker::WontReopen { number: 5 }),
            "the push closed #5, and GitHub won't reopen a PR whose branch moved while it was \
             closed"
        );
        assert_eq!(
            r(Blocker::Missed("the description of #3".into())),
            "the description of #3: jjpr couldn't read it before changing it, so it can't put \
             it back"
        );
        assert_eq!(
            r(Blocker::Absorbed(vec!["snapshot working copy".into()])),
            "while it ran, jj also recorded work that wasn't jjpr's (snapshot working copy), \
             and that work would be lost"
        );
    }

    #[test]
    fn stepping_back_names_the_operations_and_both_ways_on() {
        let op = |id: &str, d: &str| Operation {
            id: id.into(),
            description: d.into(),
        };
        let one = [op("60449576ff81aa", "describe commit 83c5")];
        assert_eq!(
            stepping_back(&one, "`jjpr submit` from 14:02", false),
            "Undid 1 jj operation since `jjpr submit` from 14:02:\n  60449576ff81 describe \
             commit 83c5\nTo put it back: jjpr redo. To undo `jjpr submit` from 14:02: jjpr undo"
        );
        let seven: Vec<Operation> = (0..7)
            .map(|i| op(&format!("{i}00000000000"), "x"))
            .collect();
        let text = stepping_back(&seven, "it", true);
        assert!(text.starts_with(
            "Would undo 7 jj operations since it, so that the next `jjpr undo` reaches it:"
        ));
        assert!(text.ends_with("\n  and 2 more"), "{text}");
        assert!(stepping_back(&seven[..2], "it", false).contains("To put them back"));
    }

    #[test]
    fn a_changed_repo_lists_the_operations_since_up_to_five() {
        let op = |i: usize| Operation {
            id: format!("{i}abcdef0123456789"),
            description: format!("describe commit {i}"),
        };
        let since: Vec<Operation> = (1..=7).map(op).collect();
        let text = reason(&Blocker::RepoChanged { since }, &entry());
        assert!(text.starts_with(
            "the repo changed since, and that work would be lost\n    jj operations since:\n      \
             1abcdef01234 describe commit 1"
        ));
        assert!(
            text.ends_with("describe commit 5\n      and 2 more"),
            "{text}"
        );
        assert_eq!(
            reason(&Blocker::RepoChanged { since: vec![] }, &entry()),
            "the repo changed since, and that work would be lost"
        );
    }

    #[test]
    fn a_refusal_without_force_says_to_rerun_with_it() {
        let e = entry();
        let blockers = [close(44, 2), close(43, 0)];
        assert_eq!(
            refused(&blockers, &e, Direction::Undo, false, e.started_at).unwrap(),
            format!(
                "jjpr can't undo all of `jjpr submit` from {} without --force, so it changed \
                 nothing:\n  - #44, which the submit opened, would be closed, and it has 2 \
                 comments or reviews from others\n  - #43, which the submit opened, would be \
                 closed\nNothing else stands in the way. Rerun with --force to undo all of it: jjpr undo --force",
                at()
            )
        );
        assert_eq!(
            refused(&blockers, &e, Direction::Undo, true, e.started_at),
            None
        );
        assert_eq!(refused(&[], &e, Direction::Undo, false, e.started_at), None);
    }

    #[test]
    fn a_refusal_force_cannot_clear_names_only_what_stops_it() {
        let e = entry();
        let blockers = [close(44, 0), moved()];
        for force in [false, true] {
            assert_eq!(
                refused(&blockers, &e, Direction::Redo, force, e.started_at).unwrap(),
                format!(
                    "jjpr can't redo all of `jjpr submit` from {}, so it changed nothing:\n  \
                     - 'auth' changed on origin after jjpr last pushed it (jjpr expected \
                     9e8f7a6b; origin has 3c4d5e6f)\nUse jj to get the stack into the state you \
                     want, then run `jjpr submit`.\nTo keep the commits on 'auth': run `jj git fetch`, rebase \
                     onto `auth@origin`, then run `jjpr submit`.",
                    at()
                )
            );
        }
    }

    #[test]
    fn a_dry_run_names_every_blocker() {
        let e = entry();
        let blockers = [close(44, 0), moved()];
        let text = dry_run_blockers(&blockers, &e, Direction::Undo, false, e.started_at).unwrap();
        assert_eq!(
            text,
            format!(
                "jjpr can't undo all of `jjpr submit` from {}, so the real run would change \
                 nothing:\n  - #44, which the submit opened, would be closed\n  - 'auth' changed \
                 on origin after jjpr last pushed it (jjpr expected 9e8f7a6b; origin has \
                 3c4d5e6f)\nUse jj to get the stack into the state you want, then run `jjpr \
                 submit`.\nTo keep the commits on 'auth': run `jj git fetch`, rebase onto \
                 `auth@origin`, then run `jjpr submit`.",
                at()
            )
        );
        let forced = [close(44, 0)];
        assert!(
            dry_run_blockers(&forced, &e, Direction::Undo, false, 0)
                .unwrap()
                .ends_with("Rerun with --force to undo all of it: jjpr undo --force")
        );
        assert_eq!(
            dry_run_blockers(&forced, &e, Direction::Undo, true, 0),
            None
        );
    }

    #[test]
    fn force_warns_about_what_it_goes_past() {
        let e = entry();
        let w = |b: Blocker| overridden(&b, &e);
        assert_eq!(w(close(44, 0)), None, "closing a quiet PR needs no warning");
        assert_eq!(
            w(close(44, 1)).unwrap(),
            "  Warning: #44 has 1 comment or review from others; closing it anyway."
        );
        assert_eq!(
            w(Blocker::Changed(Changed::Body { number: 3 })).unwrap(),
            "  Warning: the description of #3 was edited after jjpr wrote it; restoring it \
             anyway."
        );
        assert_eq!(
            w(Blocker::Changed(Changed::CommentGone { pr: 1 })).unwrap(),
            "  Warning: the stack comment on #1 was deleted after jjpr wrote it; posting it \
             again."
        );
        assert_eq!(
            w(Blocker::BaseGone {
                number: 2,
                base: "bottom".into()
            })
            .unwrap(),
            "  Warning: #2 can't go back to base 'bottom': origin no longer has that branch; \
             leaving its base as it is."
        );
        assert_eq!(w(moved()), None);
    }
}
