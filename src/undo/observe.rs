//! What the forge holds now for everything an entry touched.

use std::collections::HashMap;

use anyhow::Result;

use crate::forge::{Forge, is_open};

use super::journal::{Action, Entry};
use super::plan::{self, Observed, SeenPr, Status};
use super::{Direction, Options};

/// What the forge holds now for everything the entry touched.
pub(super) fn observe(forge: &dyn Forge, entry: &Entry, opts: Options) -> Result<Observed> {
    let (owner, repo) = (&entry.owner, &entry.repo);
    let mut observed = Observed::default();
    let mut comment_prs = Vec::new();
    let mut jjpr_comments = Vec::new();
    for record in &entry.actions {
        match &record.action {
            Action::Push { bookmark, .. } => {
                let head = forge.get_branch_head(owner, repo, bookmark)?;
                observed.branches.insert(bookmark.clone(), head);
            }
            Action::Base { before, after, .. } => {
                for base in [before, after] {
                    if !observed.branches.contains_key(base) {
                        let head = forge.get_branch_head(owner, repo, base)?;
                        observed.branches.insert(base.clone(), head);
                    }
                }
            }
            Action::CommentCreate { pr, id, .. }
            | Action::CommentUpdate { pr, id, .. }
            | Action::CommentDelete { pr, id, .. } => {
                comment_prs.push(*pr);
                jjpr_comments.push(*id);
            }
            _ => {}
        }
    }
    for number in plan::touched_prs(entry) {
        let (pr, state) = forge.get_pr(owner, repo, number)?;
        observed.prs.insert(
            number,
            SeenPr {
                base: pr.base.ref_name,
                body: pr.body.unwrap_or_default(),
                draft: pr.draft,
                status: match (state.merged, is_open(&state)) {
                    (true, _) => Status::Merged,
                    (false, true) => Status::Open,
                    (false, false) => Status::Closed,
                },
                reviewers: pr.requested_reviewers,
            },
        );
    }
    comment_prs.sort_unstable();
    comment_prs.dedup();
    for pr in comment_prs {
        let bodies: HashMap<u64, String> = forge
            .list_comments(owner, repo, pr)?
            .into_iter()
            .map(|c| (c.id, c.body.unwrap_or_default()))
            .collect();
        observed.comments.insert(pr, bodies);
    }
    if opts.force && opts.direction == Direction::Undo {
        for record in &entry.actions {
            if let Action::CreatePr { number, .. } = record.action {
                let others = forge
                    .list_comments(owner, repo, number)?
                    .iter()
                    .filter(|c| !jjpr_comments.contains(&c.id))
                    .count();
                let reviews = forge.get_pr_reviews(owner, repo, number)?;
                let reviewed =
                    reviews.approved_count as usize + usize::from(reviews.changes_requested);
                observed.activity.insert(number, others + reviewed);
            }
        }
    }
    Ok(observed)
}
