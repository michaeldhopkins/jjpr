//! What undoing or redoing one entry takes, decided from the entry and what
//! the forge holds now. Pure: the command layer gathers [`Observed`] and runs
//! the [`Plan`].

use std::collections::HashMap;

use super::journal::{Action, Entry};
use super::planner::Planner;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Undo,
    Redo,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    Open,
    #[default]
    Closed,
    Merged,
}

/// A PR as the forge has it now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeenPr {
    pub base: String,
    pub body: String,
    pub draft: bool,
    pub status: Status,
    pub reviewers: Vec<String>,
}

/// What the forge holds now, for every branch, PR and comment the entry
/// touched.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// Branch name to the commit it points at; `None`: no such branch.
    pub branches: HashMap<String, Option<String>>,
    pub prs: HashMap<u64, SeenPr>,
    /// PR number to its comments' bodies by id.
    pub comments: HashMap<u64, HashMap<u64, String>>,
    /// For each PR the entry opened: comments and reviews by anyone but jjpr.
    pub activity: HashMap<u64, usize>,
}

/// One change to make. `record` is the journal action it takes back or
/// redoes, marked when the step succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Put the local repo back to this operation, remote-tracking refs aside.
    Local {
        op: String,
    },
    Push {
        record: usize,
        bookmark: String,
        remote: String,
        from: Option<String>,
        to: Option<String>,
    },
    /// Reopen a PR that a push closed (undo) or that undo closed (redo).
    Reopen {
        record: usize,
        number: u64,
    },
    Close {
        record: usize,
        number: u64,
    },
    Base {
        record: usize,
        number: u64,
        from: String,
        to: String,
    },
    DeleteComment {
        record: usize,
        pr: u64,
        id: u64,
    },
    EditComment {
        record: usize,
        pr: u64,
        id: u64,
        to: String,
    },
    PostComment {
        record: usize,
        pr: u64,
        /// The id the comment had; the forge gives it a new one, and later
        /// steps naming this id act on that.
        id: u64,
        body: String,
    },
    Body {
        record: usize,
        number: u64,
        to: String,
    },
    Draft {
        record: usize,
        number: u64,
    },
    Ready {
        record: usize,
        number: u64,
    },
    Unrequest {
        record: usize,
        number: u64,
        who: Vec<String>,
    },
    Request {
        record: usize,
        number: u64,
        who: Vec<String>,
    },
}

/// Something the entry did that this run leaves as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// A PR the entry opened, left open without `--force`.
    OpenPr { number: u64 },
    /// Review requests already sent; withdrawing one does not unsend it.
    Notified { number: u64, who: Vec<String> },
    /// The base to put back is a branch the forge no longer has.
    BaseGone { number: u64, base: String },
}

/// A forge object someone changed after jjpr wrote it. `--force` writes over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changed {
    Base {
        number: u64,
        now: String,
    },
    Comment {
        pr: u64,
    },
    CommentGone {
        pr: u64,
    },
    Body {
        number: u64,
    },
    /// A PR the entry opened has comments or reviews from others.
    Activity {
        number: u64,
        count: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Merged {
        number: u64,
    },
    /// The PR was merged after the entry ran.
    MergedSince {
        number: u64,
    },
    BranchMoved {
        bookmark: String,
        expected: Option<String>,
        now: Option<String>,
    },
    Changed(Vec<Changed>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub kept: Vec<Kept>,
    /// What `--force` is writing over.
    pub overridden: Vec<Changed>,
    /// Records this run leaves as they are (a PR kept open, and its branch).
    pub left: Vec<usize>,
}

pub fn plan(
    entry: &Entry,
    direction: Direction,
    observed: &Observed,
    force: bool,
) -> Result<Plan, Refusal> {
    if let Some(number) = entry.merged() {
        return Err(Refusal::Merged { number });
    }
    for number in prs_needing_open(entry) {
        if observed
            .prs
            .get(&number)
            .is_some_and(|p| p.status == Status::Merged)
        {
            return Err(Refusal::MergedSince { number });
        }
    }
    let mut p = Planner {
        entry,
        direction,
        observed,
        force,
        plan: Plan::default(),
        changed: Vec::new(),
        late: Vec::new(),
        comments: observed.comments.clone(),
    };
    p.local();
    p.pushes()?;
    p.forge_writes();
    p.plan.steps.append(&mut p.late);
    if !p.changed.is_empty() {
        if !force {
            return Err(Refusal::Changed(p.changed));
        }
        p.plan.overridden = p.changed;
    }
    Ok(p.plan)
}

/// PRs whose undo only makes sense while they are unmerged: their base,
/// draft state, reviewers, or their being open. A stack comment on a PR that
/// was already merged (its history) can still be put back.
fn prs_needing_open(entry: &Entry) -> Vec<u64> {
    entry
        .actions
        .iter()
        .filter_map(|r| match &r.action {
            Action::Push { pr, .. } => *pr,
            Action::CreatePr { number, .. }
            | Action::Base { number, .. }
            | Action::Ready { number }
            | Action::Reviewers { number, .. } => Some(*number),
            _ => None,
        })
        .collect()
}

/// Every PR the entry touched.
pub fn touched_prs(entry: &Entry) -> Vec<u64> {
    let mut prs: Vec<u64> = entry
        .actions
        .iter()
        .filter_map(|r| match &r.action {
            Action::Push { pr, .. } => *pr,
            Action::CreatePr { number, .. }
            | Action::Base { number, .. }
            | Action::Body { number, .. }
            | Action::Ready { number }
            | Action::Reviewers { number, .. }
            | Action::Merge { number } => Some(*number),
            Action::CommentCreate { pr, .. }
            | Action::CommentUpdate { pr, .. }
            | Action::CommentDelete { pr, .. } => Some(*pr),
        })
        .filter(|n| *n != 0)
        .collect();
    prs.sort_unstable();
    prs.dedup();
    prs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ForgeKind;
    use crate::undo::journal::{Record, SCHEMA, State};

    fn rec(action: Action) -> Record {
        Record {
            action,
            confirmed: true,
            undone: false,
        }
    }

    fn entry(actions: Vec<Action>) -> Entry {
        Entry {
            schema: SCHEMA,
            id: "1".into(),
            command: "submit".into(),
            started_at: 0,
            remote: "origin".into(),
            forge: ForgeKind::GitHub,
            owner: "o".into(),
            repo: "r".into(),
            start_op: "start".into(),
            end_op: Some("end".into()),
            end_view: Some("v".into()),
            absorbed: vec![],
            state: State::Done,
            local_undone: false,
            undone_view: None,
            last_op: None,
            closed_by_push: Vec::new(),
            missed: Vec::new(),
            actions: actions.into_iter().map(rec).collect(),
        }
    }

    fn undone(mut e: Entry) -> Entry {
        for r in &mut e.actions {
            r.undone = true;
        }
        e.local_undone = true;
        e.state = State::Undone;
        e
    }

    fn push(bookmark: &str, before: Option<&str>, after: &str, pr: Option<u64>) -> Action {
        Action::Push {
            bookmark: bookmark.into(),
            remote: "origin".into(),
            before: before.map(Into::into),
            after: after.into(),
            pr,
        }
    }

    fn open_pr(base: &str) -> SeenPr {
        SeenPr {
            base: base.into(),
            status: Status::Open,
            ..SeenPr::default()
        }
    }

    fn branches(pairs: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(b, c)| (b.to_string(), c.map(Into::into)))
            .collect()
    }

    #[test]
    fn undo_restores_the_local_repo_first_and_pushes_the_old_head() {
        let e = entry(vec![push("a", Some("old"), "new", Some(1))]);
        let observed = Observed {
            branches: branches(&[("a", Some("new"))]),
            prs: HashMap::from([(1, open_pr("main"))]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Undo, &observed, false).unwrap();
        assert_eq!(
            plan.steps,
            vec![
                Step::Local { op: "start".into() },
                Step::Push {
                    record: 0,
                    bookmark: "a".into(),
                    remote: "origin".into(),
                    from: Some("new".into()),
                    to: Some("old".into()),
                },
            ]
        );
        assert!(plan.kept.is_empty() && plan.left.is_empty());
    }

    #[test]
    fn redo_restores_the_end_operation_and_pushes_the_new_head() {
        let e = undone(entry(vec![push("a", Some("old"), "new", None)]));
        let observed = Observed {
            branches: branches(&[("a", Some("old"))]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Redo, &observed, false).unwrap();
        assert_eq!(plan.steps[0], Step::Local { op: "end".into() });
        assert!(matches!(
            &plan.steps[1],
            Step::Push { from: Some(f), to: Some(t), .. } if f == "old" && t == "new"
        ));
    }

    #[test]
    fn a_branch_that_moved_since_refuses_even_with_force() {
        let e = entry(vec![push("a", Some("old"), "new", None)]);
        let observed = Observed {
            branches: branches(&[("a", Some("theirs"))]),
            ..Observed::default()
        };
        for force in [false, true] {
            assert_eq!(
                plan(&e, Direction::Undo, &observed, force),
                Err(Refusal::BranchMoved {
                    bookmark: "a".into(),
                    expected: Some("new".into()),
                    now: Some("theirs".into()),
                })
            );
        }
    }

    #[test]
    fn a_branch_already_where_undo_would_put_it_is_skipped() {
        let e = entry(vec![push("a", Some("old"), "new", None)]);
        let observed = Observed {
            branches: branches(&[("a", Some("old"))]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Undo, &observed, false).unwrap();
        assert_eq!(plan.steps, vec![Step::Local { op: "start".into() }]);
    }

    #[test]
    fn a_merge_is_never_undone() {
        let e = entry(vec![Action::Merge { number: 4 }]);
        for force in [false, true] {
            assert_eq!(
                plan(&e, Direction::Undo, &Observed::default(), force),
                Err(Refusal::Merged { number: 4 })
            );
        }
    }

    #[test]
    fn a_pr_merged_since_refuses() {
        let e = entry(vec![Action::Ready { number: 3 }]);
        let observed = Observed {
            prs: HashMap::from([(
                3,
                SeenPr {
                    status: Status::Merged,
                    ..SeenPr::default()
                },
            )]),
            ..Observed::default()
        };
        assert_eq!(
            plan(&e, Direction::Undo, &observed, true),
            Err(Refusal::MergedSince { number: 3 })
        );
    }

    fn created_pr_entry() -> Entry {
        entry(vec![
            push("b", None, "c1", None),
            Action::CreatePr {
                number: 7,
                head: "b".into(),
            },
        ])
    }

    fn created_pr_observed(activity: usize) -> Observed {
        Observed {
            branches: branches(&[("b", Some("c1"))]),
            prs: HashMap::from([(7, open_pr("main"))]),
            activity: HashMap::from([(7, activity)]),
            ..Observed::default()
        }
    }

    #[test]
    fn without_force_a_created_pr_and_its_branch_are_left_alone() {
        let plan = plan(
            &created_pr_entry(),
            Direction::Undo,
            &created_pr_observed(0),
            false,
        )
        .unwrap();
        assert_eq!(plan.steps, vec![Step::Local { op: "start".into() }]);
        assert_eq!(plan.kept, vec![Kept::OpenPr { number: 7 }]);
        let mut left = plan.left.clone();
        left.sort_unstable();
        assert_eq!(left, vec![0, 1]);
    }

    #[test]
    fn with_force_the_created_pr_closes_before_its_branch_is_deleted() {
        let plan = plan(
            &created_pr_entry(),
            Direction::Undo,
            &created_pr_observed(0),
            true,
        )
        .unwrap();
        assert_eq!(
            plan.steps,
            vec![
                Step::Local { op: "start".into() },
                Step::Close {
                    record: 1,
                    number: 7
                },
                Step::Push {
                    record: 0,
                    bookmark: "b".into(),
                    remote: "origin".into(),
                    from: Some("c1".into()),
                    to: None,
                },
            ]
        );
        assert!(plan.overridden.is_empty() && plan.left.is_empty());
    }

    #[test]
    fn closing_a_pr_others_have_commented_on_is_named_when_forced() {
        let e = created_pr_entry();
        let observed = created_pr_observed(2);
        let plan = plan(&e, Direction::Undo, &observed, true).unwrap();
        assert_eq!(
            plan.overridden,
            vec![Changed::Activity {
                number: 7,
                count: 2
            }]
        );
    }

    #[test]
    fn redo_pushes_the_branch_back_then_reopens_the_pr() {
        let e = undone(created_pr_entry());
        let observed = Observed {
            branches: branches(&[("b", None)]),
            prs: HashMap::from([(
                7,
                SeenPr {
                    status: Status::Closed,
                    ..SeenPr::default()
                },
            )]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Redo, &observed, false).unwrap();
        assert!(matches!(&plan.steps[1], Step::Push { to: Some(t), .. } if t == "c1"));
        assert_eq!(
            plan.steps[2],
            Step::Reopen {
                record: 1,
                number: 7
            }
        );
    }

    #[test]
    fn undo_reopens_a_pr_its_push_closed() {
        let mut e = entry(vec![push("a", Some("old"), "new", Some(5))]);
        e.closed_by_push = vec![5];
        let observed = Observed {
            branches: branches(&[("a", Some("new"))]),
            prs: HashMap::from([(5, SeenPr::default())]),
            ..Observed::default()
        };
        let p = plan(&e, Direction::Undo, &observed, false).unwrap();
        assert_eq!(
            p.steps.last(),
            Some(&Step::Reopen {
                record: 0,
                number: 5
            })
        );
        e.closed_by_push.clear();
        let p = plan(&e, Direction::Undo, &observed, false).unwrap();
        assert!(
            !p.steps.iter().any(|s| matches!(s, Step::Reopen { .. })),
            "someone else closed it: {:?}",
            p.steps
        );
    }

    fn base_entry() -> Entry {
        entry(vec![Action::Base {
            number: 2,
            before: "a".into(),
            after: "main".into(),
        }])
    }

    fn with_base(base: &str) -> Observed {
        Observed {
            prs: HashMap::from([(2, open_pr(base))]),
            ..Observed::default()
        }
    }

    #[test]
    fn a_base_is_put_back() {
        let plan = plan(&base_entry(), Direction::Undo, &with_base("main"), false).unwrap();
        assert_eq!(
            plan.steps[1],
            Step::Base {
                record: 0,
                number: 2,
                from: "main".into(),
                to: "a".into()
            }
        );
    }

    #[test]
    fn a_base_someone_changed_needs_force_and_is_named() {
        assert_eq!(
            plan(&base_entry(), Direction::Undo, &with_base("dev"), false),
            Err(Refusal::Changed(vec![Changed::Base {
                number: 2,
                now: "dev".into()
            }]))
        );
        let plan = plan(&base_entry(), Direction::Undo, &with_base("dev"), true).unwrap();
        assert_eq!(plan.overridden.len(), 1);
        assert!(matches!(plan.steps[1], Step::Base { .. }));
    }

    #[test]
    fn a_base_already_back_is_skipped() {
        let plan = plan(&base_entry(), Direction::Undo, &with_base("a"), false).unwrap();
        assert_eq!(plan.steps.len(), 1);
    }

    fn comments(pr: u64, pairs: &[(u64, &str)]) -> HashMap<u64, HashMap<u64, String>> {
        HashMap::from([(pr, pairs.iter().map(|(i, b)| (*i, b.to_string())).collect())])
    }

    #[test]
    fn comments_follow_what_jjpr_did_to_them() {
        let e = entry(vec![
            Action::CommentCreate {
                pr: 1,
                id: 10,
                body: "new".into(),
            },
            Action::CommentUpdate {
                pr: 1,
                id: 11,
                before: "old".into(),
                after: "mid".into(),
            },
            Action::CommentDelete {
                pr: 1,
                id: 12,
                body: "gone".into(),
            },
        ]);
        let observed = Observed {
            comments: comments(1, &[(10, "new"), (11, "mid")]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Undo, &observed, false).unwrap();
        assert_eq!(
            plan.steps[1..],
            [
                Step::PostComment {
                    record: 2,
                    pr: 1,
                    id: 12,
                    body: "gone".into()
                },
                Step::EditComment {
                    record: 1,
                    pr: 1,
                    id: 11,
                    to: "old".into()
                },
                Step::DeleteComment {
                    record: 0,
                    pr: 1,
                    id: 10
                },
            ]
        );
    }

    #[test]
    fn an_edited_or_deleted_comment_needs_force() {
        let e = entry(vec![Action::CommentUpdate {
            pr: 1,
            id: 11,
            before: "old".into(),
            after: "mid".into(),
        }]);
        let edited = Observed {
            comments: comments(1, &[(11, "someone's")]),
            ..Observed::default()
        };
        assert_eq!(
            plan(&e, Direction::Undo, &edited, false),
            Err(Refusal::Changed(vec![Changed::Comment { pr: 1 }]))
        );
        assert_eq!(
            plan(&e, Direction::Undo, &Observed::default(), false),
            Err(Refusal::Changed(vec![Changed::CommentGone { pr: 1 }]))
        );
    }

    #[test]
    fn a_body_is_put_back_and_an_edited_one_needs_force() {
        let e = entry(vec![Action::Body {
            number: 3,
            before: "b0".into(),
            after: "b1".into(),
        }]);
        let seen = |body: &str| Observed {
            prs: HashMap::from([(
                3,
                SeenPr {
                    body: body.into(),
                    status: Status::Open,
                    ..SeenPr::default()
                },
            )]),
            ..Observed::default()
        };
        let p = plan(&e, Direction::Undo, &seen("b1"), false).unwrap();
        assert_eq!(
            p.steps[1],
            Step::Body {
                record: 0,
                number: 3,
                to: "b0".into()
            }
        );
        assert_eq!(
            plan(&e, Direction::Undo, &seen("edited"), false),
            Err(Refusal::Changed(vec![Changed::Body { number: 3 }]))
        );
    }

    #[test]
    fn ready_goes_back_to_draft_and_forward_again() {
        let e = entry(vec![Action::Ready { number: 3 }]);
        let seen = |draft: bool| Observed {
            prs: HashMap::from([(
                3,
                SeenPr {
                    draft,
                    status: Status::Open,
                    ..SeenPr::default()
                },
            )]),
            ..Observed::default()
        };
        let p = plan(&e, Direction::Undo, &seen(false), false).unwrap();
        assert_eq!(
            p.steps[1],
            Step::Draft {
                record: 0,
                number: 3
            }
        );
        assert_eq!(
            plan(&e, Direction::Undo, &seen(true), false)
                .unwrap()
                .steps
                .len(),
            1
        );
        let p = plan(&undone(e), Direction::Redo, &seen(true), false).unwrap();
        assert_eq!(
            p.steps[1],
            Step::Ready {
                record: 0,
                number: 3
            }
        );
    }

    #[test]
    fn review_requests_still_pending_are_withdrawn_and_named_as_sent() {
        let e = entry(vec![Action::Reviewers {
            number: 3,
            added: vec!["alice".into(), "bob".into()],
        }]);
        let observed = Observed {
            prs: HashMap::from([(
                3,
                SeenPr {
                    status: Status::Open,
                    reviewers: vec!["Alice".into()],
                    ..SeenPr::default()
                },
            )]),
            ..Observed::default()
        };
        let p = plan(&e, Direction::Undo, &observed, false).unwrap();
        assert_eq!(
            p.steps[1],
            Step::Unrequest {
                record: 0,
                number: 3,
                who: vec!["alice".into()]
            }
        );
        assert_eq!(
            p.kept,
            vec![Kept::Notified {
                number: 3,
                who: vec!["alice".into()]
            }]
        );
        let p = plan(&undone(e), Direction::Redo, &observed, false).unwrap();
        assert_eq!(
            p.steps[1],
            Step::Request {
                record: 0,
                number: 3,
                who: vec!["bob".into()]
            }
        );
    }

    #[test]
    fn continuing_a_partial_undo_skips_the_local_restore() {
        let mut e = created_pr_entry();
        e.local_undone = true;
        e.state = State::PartlyUndone;
        let plan = plan(&e, Direction::Undo, &created_pr_observed(0), true).unwrap();
        assert!(matches!(plan.steps[0], Step::Close { .. }));
    }

    #[test]
    fn touched_prs_lists_each_pr_once() {
        let e = entry(vec![
            push("a", None, "c", Some(1)),
            Action::Ready { number: 1 },
            Action::CommentCreate {
                pr: 2,
                id: 0,
                body: String::new(),
            },
        ]);
        assert_eq!(touched_prs(&e), vec![1, 2]);
    }

    #[test]
    fn a_base_the_forge_no_longer_has_is_kept_and_named() {
        let mut observed = with_base("main");
        observed.branches.insert("a".into(), None);
        let p = plan(&base_entry(), Direction::Undo, &observed, false).unwrap();
        assert_eq!(p.steps.len(), 1, "only the local restore");
        assert_eq!(
            p.kept,
            vec![Kept::BaseGone {
                number: 2,
                base: "a".into()
            }]
        );
    }

    #[test]
    fn a_comment_on_a_pr_merged_before_is_still_put_back() {
        let e = entry(vec![Action::CommentUpdate {
            pr: 1,
            id: 11,
            before: "old".into(),
            after: "mid".into(),
        }]);
        let observed = Observed {
            prs: HashMap::from([(
                1,
                SeenPr {
                    status: Status::Merged,
                    ..SeenPr::default()
                },
            )]),
            comments: comments(1, &[(11, "mid")]),
            ..Observed::default()
        };
        let p = plan(&e, Direction::Undo, &observed, false).unwrap();
        assert!(matches!(p.steps[1], Step::EditComment { .. }));
    }
}
