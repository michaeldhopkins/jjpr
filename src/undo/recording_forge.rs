//! A [`Forge`] that records each write for `jjpr undo`, with the value it
//! replaced, and passes everything through.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::forge::Forge;
use crate::forge::types::{
    ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PrStatusBundle, PullRequest,
    ReviewSummary, Stack,
};

use super::journal::Action;
use super::recorder::Recorder;

pub struct RecordingForge {
    inner: Box<dyn Forge>,
    recorder: Arc<Recorder>,
}

impl RecordingForge {
    pub fn new(inner: Box<dyn Forge>, recorder: Arc<Recorder>) -> Self {
        Self { inner, recorder }
    }

    /// Run `write`, recording `action` before it and confirming or retracting
    /// it after, by how the write went.
    fn recorded<T>(&self, action: Action, write: impl FnOnce() -> Result<T>) -> Result<T> {
        let intent = self.recorder.intent(action);
        let result = write();
        match &result {
            Ok(_) => self.recorder.confirm(intent, None),
            Err(_) => self.recorder.retract(intent),
        }
        result
    }

    fn pr_ref(&self, number: u64) -> String {
        self.recorder.forge().format_ref(number)
    }

    /// A write whose prior value jjpr could not learn went through: note it,
    /// so undo can say it leaves it.
    fn unrecorded(&self, what: String, result: Result<()>) -> Result<()> {
        if result.is_ok() {
            self.recorder.missed(what);
        }
        result
    }

    /// The PR as jjpr last saw it, reading it now when this run has not.
    fn known(&self, owner: &str, repo: &str, number: u64) -> Option<super::recorder::KnownPr> {
        if self.recorder.known_pr(number).is_none()
            && let Ok((pr, state)) = self.inner.get_pr(owner, repo, number)
        {
            self.recorder.note_prs([&pr], crate::forge::is_open(&state));
        }
        self.recorder.known_pr(number)
    }
}

impl Forge for RecordingForge {
    fn list_open_prs(&self, owner: &str, repo: &str) -> Result<Vec<PullRequest>> {
        let prs = self.inner.list_open_prs(owner, repo)?;
        self.recorder.note_prs(&prs, true);
        Ok(prs)
    }

    fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        draft: bool,
    ) -> Result<PullRequest> {
        let pending = Action::CreatePr {
            number: 0,
            head: head.to_string(),
        };
        let intent = self.recorder.intent(pending);
        match self
            .inner
            .create_pr(owner, repo, title, body, head, base, draft)
        {
            Ok(pr) => {
                let created = Action::CreatePr {
                    number: pr.number,
                    head: head.to_string(),
                };
                self.recorder.confirm(intent, Some(created));
                self.recorder.note_prs([&pr], true);
                Ok(pr)
            }
            Err(e) => {
                self.recorder.retract(intent);
                Err(e)
            }
        }
    }

    fn update_pr_base(&self, owner: &str, repo: &str, number: u64, base: &str) -> Result<()> {
        let Some(before) = self.known(owner, repo, number).map(|p| p.base) else {
            let r = self.inner.update_pr_base(owner, repo, number, base);
            return self.unrecorded(format!("the base of {}", self.pr_ref(number)), r);
        };
        let action = Action::Base {
            number,
            before,
            after: base.to_string(),
        };
        self.recorded(action, || {
            self.inner.update_pr_base(owner, repo, number, base)
        })?;
        self.recorder
            .update_known_pr(number, |p| p.base = base.to_string());
        Ok(())
    }

    fn request_reviewers(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        reviewers: &[String],
    ) -> Result<()> {
        let Some(before) = self.known(owner, repo, number).map(|p| p.reviewers) else {
            let r = self.inner.request_reviewers(owner, repo, number, reviewers);
            return self.unrecorded(format!("the review requests on {}", self.pr_ref(number)), r);
        };
        let added: Vec<String> = reviewers
            .iter()
            .filter(|r| !before.iter().any(|b| b.eq_ignore_ascii_case(r)))
            .cloned()
            .collect();
        if added.is_empty() {
            return self.inner.request_reviewers(owner, repo, number, reviewers);
        }
        let action = Action::Reviewers {
            number,
            added: added.clone(),
        };
        self.recorded(action, || {
            self.inner.request_reviewers(owner, repo, number, reviewers)
        })?;
        self.recorder
            .update_known_pr(number, |p| p.reviewers.extend(added));
        Ok(())
    }

    fn list_comments(&self, owner: &str, repo: &str, number: u64) -> Result<Vec<IssueComment>> {
        let comments = self.inner.list_comments(owner, repo, number)?;
        self.recorder.note_comments(number, &comments);
        Ok(comments)
    }

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let pending = Action::CommentCreate {
            pr: number,
            id: 0,
            body: body.to_string(),
        };
        let intent = self.recorder.intent(pending);
        match self.inner.create_comment(owner, repo, number, body) {
            Ok(comment) => {
                let created = Action::CommentCreate {
                    pr: number,
                    id: comment.id,
                    body: body.to_string(),
                };
                self.recorder.confirm(intent, Some(created));
                self.recorder
                    .set_known_comment(comment.id, number, Some(body.to_string()));
                Ok(comment)
            }
            Err(e) => {
                self.recorder.retract(intent);
                Err(e)
            }
        }
    }

    fn update_comment(&self, owner: &str, repo: &str, comment_id: u64, body: &str) -> Result<()> {
        let Some((pr, before)) = self.recorder.known_comment(comment_id) else {
            let r = self.inner.update_comment(owner, repo, comment_id, body);
            return self.unrecorded("an edit to a stack comment".to_string(), r);
        };
        let action = Action::CommentUpdate {
            pr,
            id: comment_id,
            before,
            after: body.to_string(),
        };
        self.recorded(action, || {
            self.inner.update_comment(owner, repo, comment_id, body)
        })?;
        self.recorder
            .set_known_comment(comment_id, pr, Some(body.to_string()));
        Ok(())
    }

    fn delete_comment(&self, owner: &str, repo: &str, comment_id: u64) -> Result<()> {
        let Some((pr, body)) = self.recorder.known_comment(comment_id) else {
            let r = self.inner.delete_comment(owner, repo, comment_id);
            return self.unrecorded("a deleted stack comment".to_string(), r);
        };
        let action = Action::CommentDelete {
            pr,
            id: comment_id,
            body,
        };
        self.recorded(action, || {
            self.inner.delete_comment(owner, repo, comment_id)
        })?;
        self.recorder.set_known_comment(comment_id, pr, None);
        Ok(())
    }

    fn update_pr_body(&self, owner: &str, repo: &str, number: u64, body: &str) -> Result<()> {
        let Some(before) = self.known(owner, repo, number).map(|p| p.body) else {
            let r = self.inner.update_pr_body(owner, repo, number, body);
            return self.unrecorded(format!("the description of {}", self.pr_ref(number)), r);
        };
        let action = Action::Body {
            number,
            before,
            after: body.to_string(),
        };
        self.recorded(action, || {
            self.inner.update_pr_body(owner, repo, number, body)
        })?;
        self.recorder
            .update_known_pr(number, |p| p.body = body.to_string());
        Ok(())
    }

    fn mark_pr_ready(&self, owner: &str, repo: &str, number: u64) -> Result<()> {
        self.recorded(Action::Ready { number }, || {
            self.inner.mark_pr_ready(owner, repo, number)
        })
    }

    fn merge_pr(&self, owner: &str, repo: &str, number: u64, method: MergeMethod) -> Result<()> {
        self.recorded(Action::Merge { number }, || {
            self.inner.merge_pr(owner, repo, number, method)
        })
    }

    fn get_authenticated_user(&self) -> Result<String> {
        self.inner.get_authenticated_user()
    }
    fn get_authenticated_emails(&self) -> Result<Vec<String>> {
        self.inner.get_authenticated_emails()
    }
    fn find_merged_pr(&self, owner: &str, repo: &str, head: &str) -> Result<Option<PullRequest>> {
        self.inner.find_merged_pr(owner, repo, head)
    }
    fn list_recently_merged_prs(&self, owner: &str, repo: &str) -> Result<Vec<PullRequest>> {
        self.inner.list_recently_merged_prs(owner, repo)
    }
    fn get_pr_checks_status(&self, owner: &str, repo: &str, head: &str) -> Result<ChecksStatus> {
        self.inner.get_pr_checks_status(owner, repo, head)
    }
    fn get_pr_reviews(&self, owner: &str, repo: &str, number: u64) -> Result<ReviewSummary> {
        self.inner.get_pr_reviews(owner, repo, number)
    }
    fn get_pr_mergeability(&self, owner: &str, repo: &str, number: u64) -> Result<PrMergeability> {
        self.inner.get_pr_mergeability(owner, repo, number)
    }
    /// Submit reads a PR's state right after pushing to it; a PR it knew open
    /// that is now closed was closed by the push.
    fn get_pr_state(&self, owner: &str, repo: &str, number: u64) -> Result<PrState> {
        let state = self.inner.get_pr_state(owner, repo, number)?;
        if !state.merged && !crate::forge::is_open(&state) {
            self.recorder.note_closed(number);
        }
        Ok(state)
    }
    fn get_stack(&self, owner: &str, repo: &str, stack_number: u64) -> Result<Option<Stack>> {
        self.inner.get_stack(owner, repo, stack_number)
    }
    fn base_dismisses_stale_approvals(
        &self,
        owner: &str,
        repo: &str,
        base_branch: &str,
    ) -> Result<Option<bool>> {
        self.inner
            .base_dismisses_stale_approvals(owner, repo, base_branch)
    }
    fn batch_pr_status(
        &self,
        owner: &str,
        repo: &str,
        prs: &[(u64, String)],
    ) -> Option<HashMap<u64, PrStatusBundle>> {
        self.inner.batch_pr_status(owner, repo, prs)
    }
    fn get_pr(&self, owner: &str, repo: &str, number: u64) -> Result<(PullRequest, PrState)> {
        self.inner.get_pr(owner, repo, number)
    }
    fn close_pr(&self, owner: &str, repo: &str, number: u64) -> Result<()> {
        self.inner.close_pr(owner, repo, number)
    }
    fn reopen_pr(&self, owner: &str, repo: &str, number: u64) -> Result<()> {
        self.inner.reopen_pr(owner, repo, number)
    }
    fn convert_to_draft(&self, owner: &str, repo: &str, number: u64) -> Result<()> {
        self.inner.convert_to_draft(owner, repo, number)
    }
    fn remove_reviewers(&self, owner: &str, repo: &str, number: u64, who: &[String]) -> Result<()> {
        self.inner.remove_reviewers(owner, repo, number, who)
    }
    fn get_branch_head(&self, owner: &str, repo: &str, branch: &str) -> Result<Option<String>> {
        self.inner.get_branch_head(owner, repo, branch)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::forge::types::PullRequestRef;
    use crate::undo::journal::Journal;
    use crate::undo::recorder::tests::{FakeRepo, meta};

    /// A forge with one open PR (#3) carrying one comment (#9).
    struct OnePr {
        fail_writes: bool,
        next_comment: Mutex<u64>,
    }

    fn pr(number: u64) -> PullRequest {
        let side = |r: &str| PullRequestRef {
            ref_name: r.into(),
            label: String::new(),
            sha: String::new(),
        };
        PullRequest {
            number,
            html_url: String::new(),
            title: "t".into(),
            body: Some("b".into()),
            base: side("main"),
            head: side("feat"),
            draft: true,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec!["old".into()],
            author: String::new(),
            stack: None,
        }
    }

    impl OnePr {
        fn write(&self) -> Result<()> {
            if self.fail_writes {
                anyhow::bail!("HTTP 422");
            }
            Ok(())
        }
    }

    impl Forge for OnePr {
        fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
            Ok(vec![pr(3)])
        }
        fn create_pr(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<PullRequest> {
            self.write()?;
            Ok(pr(4))
        }
        fn update_pr_base(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            self.write()
        }
        fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> {
            self.write()
        }
        fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> {
            Ok(vec![IssueComment {
                id: 9,
                body: Some("c".into()),
            }])
        }
        fn create_comment(&self, _: &str, _: &str, _: u64, body: &str) -> Result<IssueComment> {
            self.write()?;
            let mut next = self.next_comment.lock().unwrap();
            *next += 1;
            Ok(IssueComment {
                id: *next,
                body: Some(body.into()),
            })
        }
        fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            self.write()
        }
        fn delete_comment(&self, _: &str, _: &str, _: u64) -> Result<()> {
            self.write()
        }
        fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> {
            self.write()
        }
        fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> {
            self.write()
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("me".into())
        }
        fn find_merged_pr(&self, _: &str, _: &str, _: &str) -> Result<Option<PullRequest>> {
            Ok(None)
        }
        fn merge_pr(&self, _: &str, _: &str, _: u64, _: MergeMethod) -> Result<()> {
            self.write()
        }
        fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
            unimplemented!()
        }
        fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
            unimplemented!()
        }
        fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
            unimplemented!()
        }
        fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
            unimplemented!()
        }
    }

    fn recording(fail_writes: bool) -> (tempfile::TempDir, Arc<Recorder>, RecordingForge) {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::start(
            Journal::at(dir.path().to_path_buf()),
            Box::new(Arc::new(FakeRepo::at("op"))),
            meta(),
        );
        let inner = OnePr {
            fail_writes,
            next_comment: Mutex::new(100),
        };
        let forge = RecordingForge::new(Box::new(inner), rec.clone());
        (dir, rec, forge)
    }

    fn actions(rec: &Recorder) -> Vec<Action> {
        rec.current()
            .unwrap()
            .actions
            .into_iter()
            .map(|r| {
                assert!(r.confirmed, "{r:?}");
                r.action
            })
            .collect()
    }

    #[test]
    fn each_write_records_the_value_it_replaced() {
        let (_dir, rec, f) = recording(false);
        f.list_open_prs("o", "r").unwrap();
        f.list_comments("o", "r", 3).unwrap();
        f.update_pr_base("o", "r", 3, "next").unwrap();
        f.update_pr_body("o", "r", 3, "b2").unwrap();
        f.update_pr_body("o", "r", 3, "b3").unwrap();
        f.update_comment("o", "r", 9, "c2").unwrap();
        f.delete_comment("o", "r", 9).unwrap();
        f.request_reviewers("o", "r", 3, &["old".into(), "new".into()])
            .unwrap();
        f.request_reviewers("o", "r", 3, &["NEW".into()]).unwrap();
        f.mark_pr_ready("o", "r", 3).unwrap();
        assert_eq!(
            actions(&rec),
            vec![
                Action::Base {
                    number: 3,
                    before: "main".into(),
                    after: "next".into()
                },
                Action::Body {
                    number: 3,
                    before: "b".into(),
                    after: "b2".into()
                },
                Action::Body {
                    number: 3,
                    before: "b2".into(),
                    after: "b3".into()
                },
                Action::CommentUpdate {
                    pr: 3,
                    id: 9,
                    before: "c".into(),
                    after: "c2".into()
                },
                Action::CommentDelete {
                    pr: 3,
                    id: 9,
                    body: "c2".into()
                },
                Action::Reviewers {
                    number: 3,
                    added: vec!["new".into()]
                },
                Action::Ready { number: 3 },
            ]
        );
    }

    #[test]
    fn created_things_are_recorded_with_their_new_ids() {
        let (_dir, rec, f) = recording(false);
        let pr = f
            .create_pr("o", "r", "t", "b", "feat", "main", false)
            .unwrap();
        let comment = f.create_comment("o", "r", pr.number, "nav").unwrap();
        f.merge_pr("o", "r", 4, MergeMethod::Squash).unwrap();
        assert_eq!(
            actions(&rec),
            vec![
                Action::CreatePr {
                    number: 4,
                    head: "feat".into()
                },
                Action::CommentCreate {
                    pr: 4,
                    id: comment.id,
                    body: "nav".into()
                },
                Action::Merge { number: 4 },
            ]
        );
        assert_eq!(rec.open_pr_for("feat"), Some(4));
    }

    #[test]
    fn a_refused_write_leaves_no_record() {
        let (_dir, rec, f) = recording(true);
        f.list_open_prs("o", "r").unwrap();
        assert!(f.update_pr_base("o", "r", 3, "x").is_err());
        assert!(f.create_pr("o", "r", "t", "b", "h", "main", false).is_err());
        assert!(f.mark_pr_ready("o", "r", 3).is_err());
        assert!(rec.current().unwrap().actions.is_empty());
    }

    #[test]
    fn a_write_to_something_never_read_passes_through_unrecorded() {
        let (_dir, rec, f) = recording(false);
        f.update_pr_base("o", "r", 7, "x").unwrap();
        f.update_comment("o", "r", 55, "x").unwrap();
        f.delete_comment("o", "r", 55).unwrap();
        assert!(rec.current().unwrap().actions.is_empty());
    }
}
