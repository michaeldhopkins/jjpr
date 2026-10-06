//! How [`super::plan::plan`] walks an entry's records, one kind at a time.

use std::collections::HashMap;

use super::journal::{Action, Entry};
use super::plan::{Changed, Direction, Kept, Observed, Plan, Refusal, SeenPr, Status, Step};

pub(super) struct Planner<'a> {
    pub(super) entry: &'a Entry,
    pub(super) direction: Direction,
    pub(super) observed: &'a Observed,
    pub(super) force: bool,
    pub(super) plan: Plan,
    pub(super) changed: Vec<Changed>,
    /// Steps that must come last: closing the PRs the entry opened, then
    /// deleting their branches (deleting first would close them as deleted).
    pub(super) late: Vec<Step>,
    /// The comments as they will be once the steps so far have run: one
    /// record can delete what an earlier one in the same entry wrote.
    pub(super) comments: HashMap<u64, HashMap<u64, String>>,
}

impl<'a> Planner<'a> {
    fn undo(&self) -> bool {
        self.direction == Direction::Undo
    }

    /// The records this run acts on, in the order it takes them: newest first
    /// to undo, oldest first to redo. Redo only touches what undo took back.
    fn records(&self) -> Vec<(usize, &'a Action)> {
        let entry: &'a Entry = self.entry;
        let mut records: Vec<(usize, &'a Action)> = entry
            .actions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.undone != self.undo())
            .map(|(i, r)| (i, &r.action))
            .collect();
        if self.undo() {
            records.reverse();
        }
        records
    }

    pub(super) fn local(&mut self) {
        let op = match self.direction {
            Direction::Undo if !self.entry.local_undone => Some(self.entry.start_op.clone()),
            Direction::Redo if self.entry.local_undone => self.entry.end_op.clone(),
            _ => None,
        };
        if let Some(op) = op {
            self.plan.steps.push(Step::Local { op });
        }
    }

    /// The PR this entry opened on `bookmark`, if it did.
    fn created_on(&self, bookmark: &str) -> Option<u64> {
        self.entry.actions.iter().find_map(|r| match &r.action {
            Action::CreatePr { number, head } if head == bookmark => Some(*number),
            _ => None,
        })
    }

    pub(super) fn pushes(&mut self) -> Result<(), Refusal> {
        for (record, action) in self.records() {
            let Action::Push {
                bookmark,
                remote,
                before,
                after,
                pr,
            } = action
            else {
                continue;
            };
            let (from, to) = match self.direction {
                Direction::Undo => (Some(after.clone()), before.clone()),
                Direction::Redo => (before.clone(), Some(after.clone())),
            };
            // Without --force, the branch of a PR the entry opened stays, and
            // so does the PR (see `forge_writes`).
            let kept_open = self.undo() && to.is_none() && self.created_on(bookmark).is_some();
            if kept_open && !self.force {
                self.plan.left.push(record);
                continue;
            }
            let now = self.observed.branches.get(bookmark).cloned().flatten();
            if now == to {
                continue;
            }
            if now != from {
                return Err(Refusal::BranchMoved {
                    bookmark: bookmark.clone(),
                    expected: from,
                    now,
                });
            }
            let step = Step::Push {
                record,
                bookmark: bookmark.clone(),
                remote: remote.clone(),
                from,
                to: to.clone(),
            };
            if kept_open {
                self.late.push(step);
            } else {
                self.plan.steps.push(step);
            }
            // Only a PR the push itself closed: one a reviewer closed on
            // purpose stays closed.
            if self.undo()
                && let Some(n) = pr
                && self.entry.closed_by_push.contains(n)
                && self
                    .observed
                    .prs
                    .get(n)
                    .is_some_and(|p| p.status == Status::Closed)
            {
                self.plan.steps.push(Step::Reopen { record, number: *n });
            }
        }
        Ok(())
    }

    fn seen(&self, number: u64) -> Option<&SeenPr> {
        self.observed.prs.get(&number)
    }

    fn comment(&self, pr: u64, id: u64) -> Option<&String> {
        self.comments.get(&pr).and_then(|c| c.get(&id))
    }

    pub(super) fn forge_writes(&mut self) {
        for (record, action) in self.records() {
            match action {
                Action::Push { .. } | Action::Merge { .. } => {}
                Action::CreatePr { number, .. } => self.created(record, *number),
                Action::Base {
                    number,
                    before,
                    after,
                } => {
                    let (from, to) = self.sides(before, after);
                    let Some(now) = self.seen(*number).map(|p| p.base.clone()) else {
                        continue;
                    };
                    if now == to {
                        continue;
                    }
                    // A PR cannot target a branch the forge no longer has: the
                    // merged base of a restack, usually.
                    if self.observed.branches.get(&to) == Some(&None) {
                        self.plan.kept.push(Kept::BaseGone {
                            number: *number,
                            base: to,
                        });
                        continue;
                    }
                    if now != from {
                        self.changed.push(Changed::Base {
                            number: *number,
                            now,
                        });
                    }
                    self.plan.steps.push(Step::Base {
                        record,
                        number: *number,
                        from,
                        to,
                    });
                }
                Action::Body {
                    number,
                    before,
                    after,
                } => {
                    let (from, to) = self.sides(before, after);
                    let Some(now) = self.seen(*number).map(|p| p.body.clone()) else {
                        continue;
                    };
                    if same(&now, &to) {
                        continue;
                    }
                    if !same(&now, &from) {
                        self.changed.push(Changed::Body { number: *number });
                    }
                    self.plan.steps.push(Step::Body {
                        record,
                        number: *number,
                        to,
                    });
                }
                Action::CommentUpdate {
                    pr,
                    id,
                    before,
                    after,
                } => {
                    let (from, to) = self.sides(before, after);
                    match self.comment(*pr, *id).cloned() {
                        None => self.changed.push(Changed::CommentGone { pr: *pr }),
                        Some(now) if same(&now, &to) => {}
                        Some(now) => {
                            if !same(&now, &from) {
                                self.changed.push(Changed::Comment { pr: *pr });
                            }
                            self.set_comment(*pr, *id, Some(to.clone()));
                            self.plan.steps.push(Step::EditComment {
                                record,
                                pr: *pr,
                                id: *id,
                                to,
                            });
                        }
                    }
                }
                Action::CommentCreate { pr, id, body } => {
                    self.comment_exists(record, *pr, *id, body, self.undo());
                }
                Action::CommentDelete { pr, id, body } => {
                    self.comment_exists(record, *pr, *id, body, !self.undo());
                }
                Action::Ready { number } => {
                    let Some(draft) = self.seen(*number).map(|p| p.draft) else {
                        continue;
                    };
                    if self.undo() && !draft {
                        self.plan.steps.push(Step::Draft {
                            record,
                            number: *number,
                        });
                    } else if !self.undo() && draft {
                        self.plan.steps.push(Step::Ready {
                            record,
                            number: *number,
                        });
                    }
                }
                Action::Reviewers { number, added } => self.reviewers(record, *number, added),
            }
        }
    }

    /// `(from, to)` for a value jjpr changed from `before` to `after`.
    fn sides(&self, before: &str, after: &str) -> (String, String) {
        match self.direction {
            Direction::Undo => (after.to_string(), before.to_string()),
            Direction::Redo => (before.to_string(), after.to_string()),
        }
    }

    /// A comment that should exist with `body` now, and should be removed
    /// (`remove`) or should not exist and be put back.
    fn comment_exists(&mut self, record: usize, pr: u64, id: u64, body: &str, remove: bool) {
        let now = self.comment(pr, id).cloned();
        if remove {
            match now {
                None => {}
                Some(now) => {
                    if !same(&now, body) {
                        self.changed.push(Changed::Comment { pr });
                    }
                    self.set_comment(pr, id, None);
                    self.plan.steps.push(Step::DeleteComment { record, pr, id });
                }
            }
        } else if now.is_none() {
            self.set_comment(pr, id, Some(body.to_string()));
            self.plan.steps.push(Step::PostComment {
                record,
                pr,
                id,
                body: body.to_string(),
            });
        }
    }

    fn set_comment(&mut self, pr: u64, id: u64, body: Option<String>) {
        let on_pr = self.comments.entry(pr).or_default();
        match body {
            Some(body) => on_pr.insert(id, body),
            None => on_pr.remove(&id),
        };
    }

    fn created(&mut self, record: usize, number: u64) {
        let open = self.seen(number).is_some_and(|p| p.status == Status::Open);
        match self.direction {
            Direction::Undo if open && !self.force => {
                self.plan.kept.push(Kept::OpenPr { number });
                self.plan.left.push(record);
            }
            Direction::Undo if open => {
                let count = self.observed.activity.get(&number).copied().unwrap_or(0);
                if count > 0 {
                    self.changed.push(Changed::Activity { number, count });
                }
                // Before any branch deletion in `late`.
                self.late.insert(0, Step::Close { record, number });
            }
            Direction::Redo if !open => self.late.push(Step::Reopen { record, number }),
            _ => {}
        }
    }

    fn reviewers(&mut self, record: usize, number: u64, added: &[String]) {
        let Some(now) = self.seen(number).map(|p| p.reviewers.clone()) else {
            return;
        };
        let requested = |r: &String| now.iter().any(|n| n.eq_ignore_ascii_case(r));
        let who: Vec<String> = added
            .iter()
            .filter(|r| requested(r) == self.undo())
            .cloned()
            .collect();
        if who.is_empty() {
            return;
        }
        if self.undo() {
            self.plan.kept.push(Kept::Notified {
                number,
                who: who.clone(),
            });
            self.plan.steps.push(Step::Unrequest {
                record,
                number,
                who,
            });
        } else {
            self.plan.steps.push(Step::Request {
                record,
                number,
                who,
            });
        }
    }
}

/// Text the forge may hand back changed in form only: GitLab drops a
/// comment's final newline, and line endings can come back as CRLF.
fn same(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace("\r\n", "\n").trim_end().to_string();
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::same;

    #[test]
    fn same_ignores_trailing_whitespace_and_line_endings_only() {
        assert!(same("a\nb\n", "a\nb"));
        assert!(same("a\r\nb", "a\nb\n\n"));
        assert!(!same("a\nb", "a\nc"));
        assert!(!same(" a", "a"));
    }
}
