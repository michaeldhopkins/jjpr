//! Putting back what a failed run had done. Every step a run takes yields
//! its inverse ([`inverse`]); when a later step fails, the executor runs the
//! inverses newest first, so the repo and the forge end as they began. A
//! check afterwards ([`differences`]) compares the forge with what it held
//! before the run, since the step that failed may have gone through anyway.

use std::collections::BTreeSet;

use super::plan::{Observed, Status, Step};
use super::planner::same;

/// The step that takes `step` back once it has run. `op_before` is the
/// operation that was current before a [`Step::Local`] ran; `posted` the id
/// the forge gave a comment a [`Step::PostComment`] posted.
pub fn inverse(step: &Step, op_before: &str, posted: Option<u64>) -> Step {
    match step.clone() {
        Step::Local { .. } => Step::Local {
            op: op_before.to_string(),
        },
        Step::Push {
            record,
            bookmark,
            remote,
            from,
            to,
        } => Step::Push {
            record,
            bookmark,
            remote,
            from: to,
            to: from,
        },
        Step::Reopen { record, number } => Step::Close { record, number },
        Step::Close { record, number } => Step::Reopen { record, number },
        Step::Base {
            record,
            number,
            from,
            to,
        } => Step::Base {
            record,
            number,
            from: to,
            to: from,
        },
        Step::DeleteComment {
            record,
            pr,
            id,
            body,
        } => Step::PostComment {
            record,
            pr,
            id,
            body,
        },
        Step::EditComment {
            record,
            pr,
            id,
            from,
            to,
        } => Step::EditComment {
            record,
            pr,
            id,
            from: to,
            to: from,
        },
        Step::PostComment {
            record,
            pr,
            id,
            body,
        } => Step::DeleteComment {
            record,
            pr,
            id: posted.unwrap_or(id),
            body,
        },
        Step::Body {
            record,
            number,
            from,
            to,
        } => Step::Body {
            record,
            number,
            from: to,
            to: from,
        },
        Step::Draft { record, number } => Step::Ready { record, number },
        Step::Ready { record, number } => Step::Draft { record, number },
        Step::Unrequest {
            record,
            number,
            who,
        } => Step::Request {
            record,
            number,
            who,
        },
        Step::Request {
            record,
            number,
            who,
        } => Step::Unrequest {
            record,
            number,
            who,
        },
    }
}

/// Something on the forge (or the local repo) that is not as it was before a
/// run, after the run put back what it had done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Difference {
    Local,
    Branch {
        name: String,
        was: Option<String>,
        now: Option<String>,
    },
    Status {
        number: u64,
        was: Status,
        now: Status,
    },
    Base {
        number: u64,
        was: String,
        now: String,
    },
    Body {
        number: u64,
    },
    Draft {
        number: u64,
        now: bool,
    },
    Reviewers {
        number: u64,
    },
    Comments {
        pr: u64,
    },
}

/// What differs between the forge `before` a run and `after` it. Comments
/// are compared by what they say, since a comment put back has a new id.
pub fn differences(before: &Observed, after: &Observed) -> Vec<Difference> {
    let mut found = Vec::new();
    let mut names: Vec<&String> = before.branches.keys().collect();
    names.sort();
    for name in names {
        let was = before.branches[name].clone();
        let now = after.branches.get(name).cloned().flatten();
        if was != now {
            found.push(Difference::Branch {
                name: name.clone(),
                was,
                now,
            });
        }
    }
    let mut numbers: Vec<&u64> = before.prs.keys().collect();
    numbers.sort();
    for &number in numbers {
        let was = &before.prs[&number];
        let Some(now) = after.prs.get(&number) else {
            continue;
        };
        if was.status != now.status {
            found.push(Difference::Status {
                number,
                was: was.status,
                now: now.status,
            });
        }
        if was.base != now.base {
            found.push(Difference::Base {
                number,
                was: was.base.clone(),
                now: now.base.clone(),
            });
        }
        if !same(&was.body, &now.body) {
            found.push(Difference::Body { number });
        }
        if was.draft != now.draft {
            found.push(Difference::Draft {
                number,
                now: now.draft,
            });
        }
        if lowered(&was.reviewers) != lowered(&now.reviewers) {
            found.push(Difference::Reviewers { number });
        }
    }
    let mut prs: Vec<&u64> = before.comments.keys().collect();
    prs.sort();
    for &pr in prs {
        let now = after.comments.get(&pr).cloned().unwrap_or_default();
        if bodies(&before.comments[&pr]) != bodies(&now) {
            found.push(Difference::Comments { pr });
        }
    }
    found
}

fn lowered(names: &[String]) -> BTreeSet<String> {
    names.iter().map(|n| n.to_lowercase()).collect()
}

fn bodies(comments: &std::collections::HashMap<u64, String>) -> Vec<String> {
    let mut all: Vec<String> = comments
        .values()
        .map(|b| b.replace("\r\n", "\n").trim_end().to_string())
        .collect();
    all.sort();
    all
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;
    use crate::undo::plan::SeenPr;
    use crate::undo::world;

    fn step() -> impl Strategy<Value = Step> {
        let text = || prop::sample::select(vec!["a", "b", "c"]).prop_map(String::from);
        let commit =
            || prop::option::of(prop::sample::select(vec!["c1", "c2"]).prop_map(String::from));
        let who = || {
            prop::collection::vec(
                prop::sample::select(vec!["alice", "bob"]).prop_map(String::from),
                1..2,
            )
        };
        prop_oneof![
            (commit(), commit()).prop_map(|(from, to)| Step::Push {
                record: 0,
                bookmark: "b".into(),
                remote: "origin".into(),
                from,
                to,
            }),
            Just(Step::Reopen {
                record: 0,
                number: 1
            }),
            Just(Step::Close {
                record: 0,
                number: 1
            }),
            (text(), text()).prop_map(|(from, to)| Step::Base {
                record: 0,
                number: 1,
                from,
                to
            }),
            (text(), text()).prop_map(|(from, to)| Step::Body {
                record: 0,
                number: 1,
                from,
                to
            }),
            Just(Step::Draft {
                record: 0,
                number: 1
            }),
            Just(Step::Ready {
                record: 0,
                number: 1
            }),
            who().prop_map(|who| Step::Unrequest {
                record: 0,
                number: 1,
                who
            }),
            who().prop_map(|who| Step::Request {
                record: 0,
                number: 1,
                who
            }),
            text().prop_map(|body| Step::DeleteComment {
                record: 0,
                pr: 1,
                id: 5,
                body
            }),
            (text(), text()).prop_map(|(from, to)| Step::EditComment {
                record: 0,
                pr: 1,
                id: 5,
                from,
                to
            }),
            text().prop_map(|body| Step::PostComment {
                record: 0,
                pr: 1,
                id: 5,
                body
            }),
        ]
    }

    /// The forge exactly as `step` expects to find it: every value it
    /// replaces is there.
    fn before(step: &Step) -> Observed {
        let mut o = Observed::default();
        let mut pr = SeenPr::default();
        let mut comments = HashMap::new();
        match step {
            Step::Push { bookmark, from, .. } => {
                o.branches.insert(bookmark.clone(), from.clone());
            }
            Step::Reopen { .. } => pr.status = Status::Closed,
            Step::Close { .. } => pr.status = Status::Open,
            Step::Base { from, .. } => pr.base = from.clone(),
            Step::Body { from, .. } => pr.body = from.clone(),
            Step::Draft { .. } => pr.draft = false,
            Step::Ready { .. } => pr.draft = true,
            Step::Unrequest { who, .. } => pr.reviewers = who.clone(),
            Step::Request { .. } => {}
            Step::DeleteComment { id, body, .. } => {
                comments.insert(*id, body.clone());
            }
            Step::EditComment { id, from, .. } => {
                comments.insert(*id, from.clone());
            }
            Step::PostComment { .. } | Step::Local { .. } => {}
        }
        o.prs.insert(1, pr);
        o.comments.insert(1, comments);
        o
    }

    proptest! {
        /// A step and then its inverse leave the forge as it was, whatever the
        /// step, with the posted comment's new id taken into account.
        #[test]
        fn a_step_then_its_inverse_changes_nothing(step in step()) {
            let start = before(&step);
            let mut forge = start.clone();
            world::apply(&mut forge, &step);
            let back = inverse(&step, "op0", Some(5));
            world::apply(&mut forge, &back);
            prop_assert_eq!(differences(&start, &forge), vec![]);
        }

        /// Undoing an inverse gives the step back.
        #[test]
        fn the_inverse_of_an_inverse_is_the_step(step in step()) {
            let posted = match &step {
                Step::PostComment { id, .. } | Step::DeleteComment { id, .. } => Some(*id),
                _ => None,
            };
            prop_assert_eq!(inverse(&inverse(&step, "op0", posted), "op0", posted), step);
        }
    }

    #[test]
    fn the_local_restore_goes_back_to_the_operation_before_it() {
        let step = Step::Local { op: "start".into() };
        assert_eq!(
            inverse(&step, "op9", None),
            Step::Local { op: "op9".into() }
        );
    }

    #[test]
    fn a_posted_comment_is_taken_back_under_its_new_id() {
        let post = Step::PostComment {
            record: 2,
            pr: 1,
            id: 5,
            body: "b".into(),
        };
        assert_eq!(
            inverse(&post, "", Some(77)),
            Step::DeleteComment {
                record: 2,
                pr: 1,
                id: 77,
                body: "b".into()
            }
        );
    }

    fn pr(status: Status, base: &str, body: &str, draft: bool, reviewers: &[&str]) -> SeenPr {
        SeenPr {
            base: base.into(),
            body: body.into(),
            draft,
            status,
            reviewers: reviewers.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn differences_name_each_thing_not_as_it_was() {
        let mut was = Observed::default();
        was.branches.insert("a".into(), Some("c1".into()));
        was.prs
            .insert(1, pr(Status::Open, "main", "x", false, &["Alice"]));
        was.comments
            .insert(1, HashMap::from([(5, "s".to_string())]));
        let mut now = was.clone();
        assert_eq!(differences(&was, &now), vec![]);
        now.prs.get_mut(&1).unwrap().reviewers = vec!["alice".into()];
        now.prs.get_mut(&1).unwrap().body = "x\n".into();
        now.comments
            .insert(1, HashMap::from([(6, "s\r\n".to_string())]));
        assert_eq!(
            differences(&was, &now),
            vec![],
            "case, a new comment id and trailing whitespace are not differences"
        );
        now.branches.insert("a".into(), None);
        now.prs.insert(1, pr(Status::Closed, "dev", "y", true, &[]));
        now.comments.insert(1, HashMap::new());
        assert_eq!(
            differences(&was, &now),
            vec![
                Difference::Branch {
                    name: "a".into(),
                    was: Some("c1".into()),
                    now: None
                },
                Difference::Status {
                    number: 1,
                    was: Status::Open,
                    now: Status::Closed
                },
                Difference::Base {
                    number: 1,
                    was: "main".into(),
                    now: "dev".into()
                },
                Difference::Body { number: 1 },
                Difference::Draft {
                    number: 1,
                    now: true
                },
                Difference::Reviewers { number: 1 },
                Difference::Comments { pr: 1 },
            ]
        );
    }

    #[test]
    fn a_pr_the_second_look_did_not_see_is_not_a_difference() {
        let mut was = Observed::default();
        was.prs.insert(1, SeenPr::default());
        assert_eq!(differences(&was, &Observed::default()), vec![]);
    }
}
