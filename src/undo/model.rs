//! What a step does to a model of the forge, for generating valid plans in
//! the executor's property tests. Test-only.

use super::plan::{Observed, Status, Step};

/// What `step` does to a forge holding `forge`, as the executor would do it.
/// For generating valid plans.
pub fn apply(forge: &mut Observed, step: &Step) {
    match step {
        Step::Local { .. } => {}
        Step::Push { bookmark, to, .. } => {
            forge.branches.insert(bookmark.clone(), to.clone());
        }
        Step::Reopen { number, .. } => forge.prs.entry(*number).or_default().status = Status::Open,
        Step::Close { number, .. } => forge.prs.entry(*number).or_default().status = Status::Closed,
        Step::Base { number, to, .. } => {
            forge.prs.entry(*number).or_default().base = to.clone();
        }
        Step::Body { number, to, .. } => {
            forge.prs.entry(*number).or_default().body = to.clone();
        }
        Step::Draft { number, .. } => forge.prs.entry(*number).or_default().draft = true,
        Step::Ready { number, .. } => forge.prs.entry(*number).or_default().draft = false,
        Step::Unrequest { number, who, .. } => {
            let p = forge.prs.entry(*number).or_default();
            p.reviewers
                .retain(|r| !who.iter().any(|w| w.eq_ignore_ascii_case(r)));
        }
        Step::Request { number, who, .. } => {
            forge
                .prs
                .entry(*number)
                .or_default()
                .reviewers
                .extend(who.iter().cloned());
        }
        Step::DeleteComment { pr, id, .. } => {
            forge.comments.entry(*pr).or_default().remove(id);
        }
        Step::EditComment { pr, id, to, .. } => {
            forge
                .comments
                .entry(*pr)
                .or_default()
                .insert(*id, to.clone());
        }
        // Under the id the plan knows it by; the executor maps that to the
        // forge's new one.
        Step::PostComment { pr, id, body, .. } => {
            forge
                .comments
                .entry(*pr)
                .or_default()
                .insert(*id, body.clone());
        }
    }
}
