//! What undoing or redoing one entry takes, decided from the entry and what
//! the forge holds now. Pure: the command layer gathers [`Observed`] and runs
//! the [`Plan`].
//!
//! A plan is all or nothing. Everything that would stop part of it is a
//! [`Blocker`], and a real run starts only when no blocker is left (`--force`
//! clears the ones it may). A dry run shows the steps and the blockers both.

use std::collections::HashMap;

use super::journal::{Action, Entry};
use super::planner::Planner;
use super::repo::Operation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Undo,
    Redo,
}

impl Direction {
    pub fn opposite(self) -> Self {
        match self {
            Self::Undo => Self::Redo,
            Self::Redo => Self::Undo,
        }
    }
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
    /// Redo: for each PR whose branch it pushes, the approvals that push would
    /// dismiss.
    pub approvals: HashMap<u64, u32>,
}

/// One change to make. `record` is the journal action it takes back or
/// redoes, marked when the step succeeds. Each step carries what it replaces,
/// so a run that fails partway can put back the steps it already took
/// ([`super::rollback::inverse`]).
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
        /// What the comment says now.
        body: String,
    },
    EditComment {
        record: usize,
        pr: u64,
        id: u64,
        from: String,
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
        from: String,
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

/// Something done that no undo can take back, named so nobody expects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// Review requests already sent; withdrawing one does not unsend it.
    Notified { number: u64, who: Vec<String> },
    /// Redo: approvals the push to this PR's branch will dismiss.
    ApprovalsDismissed { number: u64, count: u32 },
    /// Redo: a PR it reopens has comments or reviews from others.
    Activity { number: u64, count: usize },
    /// Redo: review requests it sends again.
    RequestedAgain { number: u64, who: Vec<String> },
}

/// A forge object someone changed after jjpr wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changed {
    Base { number: u64, now: String },
    Comment { pr: u64 },
    CommentGone { pr: u64 },
    Body { number: u64 },
}

/// Why a run cannot take back (or put back) the whole entry. The first group
/// `--force` clears; the rest nothing does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// Someone changed a forge object jjpr wrote; `--force` writes over it.
    Changed(Changed),
    /// Undo would close a PR the entry opened. A PR is public, so closing one
    /// takes `--force`. `activity`: comments and reviews from others.
    Close { number: u64, activity: usize },
    /// The base to put back is a branch the forge no longer has; `--force`
    /// leaves the PR on the base it has now.
    BaseGone { number: u64, base: String },
    /// The entry merged a PR.
    Merged { number: u64 },
    /// A PR the entry acted on was merged since.
    MergedSince { number: u64 },
    /// Someone pushed to a branch since jjpr did.
    BranchMoved {
        bookmark: String,
        expected: Option<String>,
        now: Option<String>,
    },
    /// GitHub will not reopen a PR whose branch moved while it was closed,
    /// which is what undoing the push that closed it would need.
    WontReopen { number: u64 },
    /// A write jjpr made without learning what it replaced.
    Missed(String),
    /// The local repo changed since: undoing would discard it.
    RepoChanged { since: Vec<Operation> },
    /// jj recorded work that was not jjpr's while the command ran.
    Absorbed(Vec<String>),
    /// Undo would step back over jj work since the command, and that takes
    /// these edits off the disk; `--force` goes ahead (`jjpr redo` brings
    /// them back).
    EditsOnDisk { files: Vec<String> },
    /// The work since the command reached beyond this workspace's local repo
    /// (a fetch, a push, another workspace), so undo cannot step back over it.
    NotLocal { what: String },
    /// Redo cannot reopen a PR whose base branch the forge no longer has.
    ReopenBaseGone { number: u64, base: String },
}

impl Blocker {
    /// Whether `--force` clears it.
    pub fn forceable(&self) -> bool {
        matches!(
            self,
            Self::Changed(_)
                | Self::Close { .. }
                | Self::BaseGone { .. }
                | Self::EditsOnDisk { .. }
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub kept: Vec<Kept>,
    pub blockers: Vec<Blocker>,
}

impl Plan {
    /// Whether a real run may start: every blocker is one `force` clears.
    pub fn runs(&self, force: bool) -> bool {
        self.blockers.iter().all(|b| force && b.forceable())
    }
}

/// The whole plan for `direction`, with every blocker found. Steps a blocker
/// stands in for are left out, so a dry run lists what would go through.
pub fn plan(entry: &Entry, direction: Direction, observed: &Observed) -> Plan {
    let mut p = Planner {
        entry,
        direction,
        observed,
        plan: Plan::default(),
        late: Vec::new(),
        comments: observed.comments.clone(),
    };
    if let Some(number) = entry.merged() {
        p.plan.blockers.push(Blocker::Merged { number });
    }
    for number in prs_needing_open(entry) {
        let merged = observed
            .prs
            .get(&number)
            .is_some_and(|p| p.status == Status::Merged);
        if merged && !p.plan.blockers.contains(&Blocker::MergedSince { number }) {
            p.plan.blockers.push(Blocker::MergedSince { number });
        }
    }
    if direction == Direction::Undo {
        for what in &entry.missed {
            p.plan.blockers.push(Blocker::Missed(what.clone()));
        }
    }
    p.local();
    p.pushes();
    p.forge_writes();
    p.plan.steps.append(&mut p.late);
    p.plan
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

    fn local() -> Step {
        Step::Local { op: "start".into() }
    }

    #[test]
    fn undo_restores_the_local_repo_first_and_pushes_the_old_head() {
        let e = entry(vec![push("a", Some("old"), "new", Some(1))]);
        let observed = Observed {
            branches: branches(&[("a", Some("new"))]),
            prs: HashMap::from([(1, open_pr("main"))]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Undo, &observed);
        assert_eq!(
            plan.steps,
            vec![
                local(),
                Step::Push {
                    record: 0,
                    bookmark: "a".into(),
                    remote: "origin".into(),
                    from: Some("new".into()),
                    to: Some("old".into()),
                },
            ]
        );
        assert!(plan.blockers.is_empty() && plan.runs(false));
    }

    #[test]
    fn redo_restores_the_end_operation_and_pushes_the_new_head() {
        let e = undone(entry(vec![push("a", Some("old"), "new", None)]));
        let observed = Observed {
            branches: branches(&[("a", Some("old"))]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Redo, &observed);
        assert_eq!(plan.steps[0], Step::Local { op: "end".into() });
        assert!(matches!(
            &plan.steps[1],
            Step::Push { from: Some(f), to: Some(t), .. } if f == "old" && t == "new"
        ));
    }

    #[test]
    fn a_branch_that_moved_since_blocks_even_with_force() {
        let e = entry(vec![push("a", Some("old"), "new", None)]);
        let observed = Observed {
            branches: branches(&[("a", Some("theirs"))]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Undo, &observed);
        assert_eq!(
            plan.blockers,
            vec![Blocker::BranchMoved {
                bookmark: "a".into(),
                expected: Some("new".into()),
                now: Some("theirs".into()),
            }]
        );
        assert!(!plan.runs(false) && !plan.runs(true));
        assert_eq!(plan.steps, vec![local()], "the push it blocks is left out");
    }

    #[test]
    fn a_branch_already_where_undo_would_put_it_is_skipped() {
        let e = entry(vec![push("a", Some("old"), "new", None)]);
        let observed = Observed {
            branches: branches(&[("a", Some("old"))]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Undo, &observed);
        assert_eq!(plan.steps, vec![local()]);
        assert!(plan.runs(false));
    }

    #[test]
    fn a_merge_is_never_undone() {
        let e = entry(vec![Action::Merge { number: 4 }]);
        let plan = plan(&e, Direction::Undo, &Observed::default());
        assert_eq!(plan.blockers, vec![Blocker::Merged { number: 4 }]);
        assert!(!plan.runs(true));
    }

    #[test]
    fn a_pushed_branch_whose_pr_merged_since_blocks() {
        let e = entry(vec![push("a", Some("old"), "new", Some(3))]);
        let observed = Observed {
            branches: branches(&[("a", Some("new"))]),
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
            plan(&e, Direction::Undo, &observed).blockers,
            vec![Blocker::MergedSince { number: 3 }]
        );
    }

    #[test]
    fn a_pr_merged_since_blocks_once() {
        let e = entry(vec![
            Action::Ready { number: 3 },
            Action::Base {
                number: 3,
                before: "a".into(),
                after: "main".into(),
            },
        ]);
        let observed = Observed {
            prs: HashMap::from([(
                3,
                SeenPr {
                    status: Status::Merged,
                    base: "main".into(),
                    ..SeenPr::default()
                },
            )]),
            ..Observed::default()
        };
        let plan = plan(&e, Direction::Undo, &observed);
        assert_eq!(plan.blockers, vec![Blocker::MergedSince { number: 3 }]);
        assert!(!plan.runs(true));
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
    fn closing_a_created_pr_needs_force_and_comes_before_deleting_its_branch() {
        let plan = plan(
            &created_pr_entry(),
            Direction::Undo,
            &created_pr_observed(0),
        );
        assert_eq!(
            plan.steps,
            vec![
                local(),
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
        assert_eq!(
            plan.blockers,
            vec![Blocker::Close {
                number: 7,
                activity: 0
            }]
        );
        assert!(!plan.runs(false), "no partial undo that leaves the PR open");
        assert!(plan.runs(true));
    }

    #[test]
    fn closing_a_pr_others_have_commented_on_names_the_activity() {
        let plan = plan(
            &created_pr_entry(),
            Direction::Undo,
            &created_pr_observed(2),
        );
        assert_eq!(
            plan.blockers,
            vec![Blocker::Close {
                number: 7,
                activity: 2
            }]
        );
    }

    #[test]
    fn a_created_pr_someone_closed_already_needs_nothing() {
        let mut observed = created_pr_observed(0);
        observed.prs.get_mut(&7).unwrap().status = Status::Closed;
        let plan = plan(&created_pr_entry(), Direction::Undo, &observed);
        assert!(plan.blockers.is_empty());
        assert!(matches!(
            plan.steps.last(),
            Some(Step::Push { to: None, .. })
        ));
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
        let plan = plan(&e, Direction::Redo, &observed);
        assert!(matches!(&plan.steps[1], Step::Push { to: Some(t), .. } if t == "c1"));
        assert_eq!(
            plan.steps[2],
            Step::Reopen {
                record: 1,
                number: 7
            }
        );
        assert!(plan.blockers.is_empty());
    }

    fn closed_by_push(forge: ForgeKind) -> Entry {
        let mut e = entry(vec![push("a", Some("old"), "new", Some(5))]);
        e.closed_by_push = vec![5];
        e.forge = forge;
        e
    }

    fn closed_observed() -> Observed {
        Observed {
            branches: branches(&[("a", Some("new"))]),
            prs: HashMap::from([(5, SeenPr::default())]),
            ..Observed::default()
        }
    }

    #[test]
    fn undo_reopens_a_pr_its_push_closed_where_the_forge_allows() {
        for forge in [ForgeKind::GitLab, ForgeKind::Forgejo] {
            let p = plan(&closed_by_push(forge), Direction::Undo, &closed_observed());
            assert_eq!(
                p.steps.last(),
                Some(&Step::Reopen {
                    record: 0,
                    number: 5
                })
            );
            assert!(p.blockers.is_empty());
        }
    }

    #[test]
    fn github_will_not_reopen_so_undo_cannot_take_it_back() {
        let p = plan(
            &closed_by_push(ForgeKind::GitHub),
            Direction::Undo,
            &closed_observed(),
        );
        assert_eq!(p.blockers, vec![Blocker::WontReopen { number: 5 }]);
        assert!(!p.runs(true));
    }

    #[test]
    fn a_pr_someone_else_closed_stays_closed() {
        let mut e = closed_by_push(ForgeKind::GitLab);
        e.closed_by_push.clear();
        let p = plan(&e, Direction::Undo, &closed_observed());
        assert!(
            !p.steps.iter().any(|s| matches!(s, Step::Reopen { .. })),
            "{:?}",
            p.steps
        );
        assert!(p.blockers.is_empty());
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
        let plan = plan(&base_entry(), Direction::Undo, &with_base("main"));
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
        let plan = plan(&base_entry(), Direction::Undo, &with_base("dev"));
        assert_eq!(
            plan.blockers,
            vec![Blocker::Changed(Changed::Base {
                number: 2,
                now: "dev".into()
            })]
        );
        assert!(!plan.runs(false) && plan.runs(true));
        assert!(
            matches!(&plan.steps[1], Step::Base { from, .. } if from == "dev"),
            "the step carries what is there now, so a failed run can put it back"
        );
    }

    #[test]
    fn a_base_already_back_is_skipped() {
        let plan = plan(&base_entry(), Direction::Undo, &with_base("a"));
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn a_base_the_forge_no_longer_has_needs_force() {
        let mut observed = with_base("main");
        observed.branches.insert("a".into(), None);
        let p = plan(&base_entry(), Direction::Undo, &observed);
        assert_eq!(p.steps.len(), 1, "only the local restore");
        assert_eq!(
            p.blockers,
            vec![Blocker::BaseGone {
                number: 2,
                base: "a".into()
            }]
        );
        assert!(!p.runs(false) && p.runs(true));
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
        let plan = plan(&e, Direction::Undo, &observed);
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
                    from: "mid".into(),
                    to: "old".into()
                },
                Step::DeleteComment {
                    record: 0,
                    pr: 1,
                    id: 10,
                    body: "new".into()
                },
            ]
        );
        assert!(plan.blockers.is_empty());
    }

    #[test]
    fn an_edited_comment_needs_force_and_a_deleted_one_is_posted_again_with_it() {
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
        let p = plan(&e, Direction::Undo, &edited);
        assert_eq!(
            p.blockers,
            vec![Blocker::Changed(Changed::Comment { pr: 1 })]
        );
        let p = plan(&e, Direction::Undo, &Observed::default());
        assert_eq!(
            p.blockers,
            vec![Blocker::Changed(Changed::CommentGone { pr: 1 })]
        );
        assert_eq!(
            p.steps[1],
            Step::PostComment {
                record: 0,
                pr: 1,
                id: 11,
                body: "old".into()
            }
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
        let p = plan(&e, Direction::Undo, &seen("b1"));
        assert_eq!(
            p.steps[1],
            Step::Body {
                record: 0,
                number: 3,
                from: "b1".into(),
                to: "b0".into()
            }
        );
        let p = plan(&e, Direction::Undo, &seen("edited"));
        assert_eq!(
            p.blockers,
            vec![Blocker::Changed(Changed::Body { number: 3 })]
        );
    }

    #[test]
    fn every_blocker_is_found_not_only_the_first() {
        let e = entry(vec![
            push("a", Some("old"), "new", None),
            Action::Body {
                number: 3,
                before: "b0".into(),
                after: "b1".into(),
            },
        ]);
        let mut observed = Observed {
            branches: branches(&[("a", Some("theirs"))]),
            ..Observed::default()
        };
        observed.prs.insert(
            3,
            SeenPr {
                body: "edited".into(),
                status: Status::Open,
                ..SeenPr::default()
            },
        );
        let p = plan(&e, Direction::Undo, &observed);
        assert_eq!(p.blockers.len(), 2, "{:?}", p.blockers);
        assert!(p.steps.iter().any(|s| matches!(s, Step::Body { .. })));
    }

    #[test]
    fn a_write_jjpr_could_not_record_blocks_undo_but_not_redo() {
        let mut e = entry(vec![]);
        e.missed = vec!["the description of #3".into()];
        let p = plan(&e, Direction::Undo, &Observed::default());
        assert_eq!(
            p.blockers,
            vec![Blocker::Missed("the description of #3".into())]
        );
        assert!(!p.runs(true));
        assert!(
            plan(&undone(e), Direction::Redo, &Observed::default())
                .blockers
                .is_empty()
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
        let p = plan(&e, Direction::Undo, &seen(false));
        assert_eq!(
            p.steps[1],
            Step::Draft {
                record: 0,
                number: 3
            }
        );
        assert_eq!(plan(&e, Direction::Undo, &seen(true)).steps.len(), 1);
        let p = plan(&undone(e), Direction::Redo, &seen(true));
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
        let p = plan(&e, Direction::Undo, &observed);
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
        let p = plan(&undone(e), Direction::Redo, &observed);
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
        let plan = plan(&e, Direction::Undo, &created_pr_observed(0));
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
        let p = plan(&e, Direction::Undo, &observed);
        assert!(matches!(p.steps[1], Step::EditComment { .. }));
        assert!(p.blockers.is_empty());
    }

    #[test]
    fn only_the_forceable_blockers_are_cleared_by_force() {
        let close = Blocker::Close {
            number: 1,
            activity: 0,
        };
        let moved = Blocker::WontReopen { number: 1 };
        let plan = |blockers: Vec<Blocker>| Plan {
            blockers,
            ..Plan::default()
        };
        assert!(plan(vec![]).runs(false));
        assert!(!plan(vec![close.clone()]).runs(false));
        assert!(plan(vec![close.clone()]).runs(true));
        assert!(!plan(vec![close, moved]).runs(true));
        assert_eq!(Direction::Undo.opposite(), Direction::Redo);
        assert_eq!(Direction::Redo.opposite(), Direction::Undo);
    }

    fn redo_created(base_there: Option<&str>, pushes_base: bool) -> Plan {
        let mut actions = vec![
            push("b", None, "c1", None),
            Action::CreatePr {
                number: 7,
                head: "b".into(),
            },
        ];
        if pushes_base {
            actions.insert(0, push("a", None, "c0", None));
        }
        let e = undone(entry(actions));
        let mut observed = Observed {
            branches: branches(&[("b", None), ("a", base_there)]),
            prs: HashMap::from([(
                7,
                SeenPr {
                    base: "a".into(),
                    status: Status::Closed,
                    ..SeenPr::default()
                },
            )]),
            activity: HashMap::from([(7, 2)]),
            ..Observed::default()
        };
        observed.approvals.insert(7, 1);
        plan(&e, Direction::Redo, &observed)
    }

    #[test]
    fn redo_will_not_reopen_onto_a_base_branch_the_forge_lost() {
        let p = redo_created(None, false);
        assert_eq!(
            p.blockers,
            vec![Blocker::ReopenBaseGone {
                number: 7,
                base: "a".into()
            }]
        );
        assert!(!p.runs(true));
        assert!(!p.steps.iter().any(|s| matches!(s, Step::Reopen { .. })));
    }

    #[test]
    fn a_base_the_same_redo_pushes_back_does_not_block() {
        let p = redo_created(None, true);
        assert!(p.blockers.is_empty(), "{:?}", p.blockers);
        assert!(matches!(
            p.steps.last(),
            Some(Step::Reopen { number: 7, .. })
        ));
    }

    #[test]
    fn redo_names_what_it_cannot_put_back_without_blocking() {
        let p = redo_created(Some("c0"), false);
        assert!(p.blockers.is_empty());
        assert_eq!(
            p.kept,
            vec![
                Kept::ApprovalsDismissed {
                    number: 7,
                    count: 1
                },
                Kept::Activity {
                    number: 7,
                    count: 2
                },
            ]
        );
        let p = plan(
            &undone(created_pr_entry()),
            Direction::Undo,
            &created_pr_observed(0),
        );
        assert!(
            !p.kept
                .iter()
                .any(|k| matches!(k, Kept::ApprovalsDismissed { .. } | Kept::Activity { .. })),
            "undo names neither"
        );
    }

    #[test]
    fn redo_names_review_requests_it_sends_again() {
        let e = undone(entry(vec![Action::Reviewers {
            number: 3,
            added: vec!["bob".into()],
        }]));
        let observed = Observed {
            prs: HashMap::from([(3, open_pr("main"))]),
            ..Observed::default()
        };
        let p = plan(&e, Direction::Redo, &observed);
        assert_eq!(
            p.kept,
            vec![Kept::RequestedAgain {
                number: 3,
                who: vec!["bob".into()]
            }]
        );
    }

    #[test]
    fn edits_on_disk_need_force_and_the_rest_nothing_clears() {
        let edits = Blocker::EditsOnDisk {
            files: vec!["a.txt".into()],
        };
        assert!(edits.forceable());
        assert!(
            !Blocker::NotLocal {
                what: String::new()
            }
            .forceable()
        );
        assert!(
            !Blocker::ReopenBaseGone {
                number: 1,
                base: String::new()
            }
            .forceable()
        );
    }

    #[test]
    fn redo_restores_the_local_repo_only_when_undo_had() {
        let mut e = undone(entry(vec![Action::Ready { number: 3 }]));
        e.local_undone = false;
        e.state = State::PartlyUndone;
        let p = plan(&e, Direction::Redo, &Observed::default());
        assert!(!p.steps.iter().any(|s| matches!(s, Step::Local { .. })));
    }

    #[test]
    fn only_the_branch_of_a_pr_the_entry_opened_waits_for_its_close() {
        let e = entry(vec![
            push("a", None, "c0", None),
            push("b", None, "c1", None),
            Action::CreatePr {
                number: 7,
                head: "b".into(),
            },
        ]);
        let observed = Observed {
            branches: branches(&[("a", Some("c0")), ("b", Some("c1"))]),
            prs: HashMap::from([(7, open_pr("main"))]),
            ..Observed::default()
        };
        let order: Vec<String> = plan(&e, Direction::Undo, &observed)
            .steps
            .iter()
            .map(|s| match s {
                Step::Local { .. } => "local".to_string(),
                Step::Push { bookmark, .. } => format!("push {bookmark}"),
                Step::Close { number, .. } => format!("close {number}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(order, ["local", "push a", "close 7", "push b"]);
    }

    #[test]
    fn no_approvals_to_dismiss_is_not_worth_a_note() {
        let e = undone(entry(vec![push("a", Some("old"), "new", Some(5))]));
        let mut observed = Observed {
            branches: branches(&[("a", Some("old"))]),
            prs: HashMap::from([(5, open_pr("main"))]),
            ..Observed::default()
        };
        observed.approvals.insert(5, 0);
        assert!(plan(&e, Direction::Redo, &observed).kept.is_empty());
        observed.approvals.insert(5, 2);
        assert_eq!(
            plan(&e, Direction::Redo, &observed).kept,
            vec![Kept::ApprovalsDismissed {
                number: 5,
                count: 2
            }]
        );
    }
}
