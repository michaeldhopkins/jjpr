//! A [`Jj`] that records each push for `jjpr undo`, and passes everything else
//! straight through.

use std::sync::Arc;

use anyhow::Result;

use crate::jj::Jj;
use crate::jj::types::{Bookmark, GitRemote, LogEntry};

use super::journal::Action;
use super::recorder::Recorder;

pub struct RecordingJj<J> {
    inner: J,
    recorder: Arc<Recorder>,
}

impl<J: Jj> RecordingJj<J> {
    pub fn new(inner: J, recorder: Arc<Recorder>) -> Self {
        Self { inner, recorder }
    }
}

impl<J> Drop for RecordingJj<J> {
    fn drop(&mut self) {
        self.recorder.finish();
    }
}

impl<J: Jj> Jj for RecordingJj<J> {
    fn push_bookmark(&self, name: &str, remote: &str) -> Result<()> {
        let intent = match self.recorder.repo().targets(name, remote) {
            Ok(targets) => targets.local.map(|after| {
                self.recorder.intent(Action::Push {
                    bookmark: name.to_string(),
                    remote: remote.to_string(),
                    before: targets.remote,
                    after,
                    pr: self.recorder.open_pr_for(name),
                })
            }),
            Err(_) => {
                self.recorder.missed(format!("the push of '{name}'"));
                None
            }
        }
        .flatten();
        match self
            .recorder
            .around(|| self.inner.push_bookmark(name, remote))
        {
            Ok(()) => {
                self.recorder.confirm(intent, None);
                Ok(())
            }
            Err(e) => {
                self.recorder.retract(intent);
                Err(e)
            }
        }
    }

    fn checkpoint(&self) {
        self.recorder.checkpoint();
        self.inner.checkpoint();
    }

    fn git_fetch(&self) -> Result<()> {
        self.recorder.around_fetch(|| self.inner.git_fetch())
    }
    fn get_user_email(&self) -> Result<String> {
        self.inner.get_user_email()
    }
    fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
        self.inner.get_my_bookmarks()
    }
    fn get_status_bookmarks(&self, all_owned_stacks: bool) -> Result<Vec<Bookmark>> {
        self.inner.get_status_bookmarks(all_owned_stacks)
    }
    fn get_changes_to_commit(&self, to_commit_id: &str) -> Result<Vec<LogEntry>> {
        self.inner.get_changes_to_commit(to_commit_id)
    }
    fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
        self.inner.get_git_remotes()
    }
    fn get_default_branch(&self) -> Result<String> {
        self.inner.get_default_branch()
    }
    fn get_working_copy_commit_id(&self) -> Result<String> {
        self.inner.get_working_copy_commit_id()
    }
    fn rebase_onto(&self, source: &str, destination: &str) -> Result<()> {
        self.recorder
            .around(|| self.inner.rebase_onto(source, destination))
    }
    fn rebase_onto_skipping_emptied(&self, source: &str, destination: &str) -> Result<()> {
        self.recorder
            .around(|| self.inner.rebase_onto_skipping_emptied(source, destination))
    }
    fn rebase_onto_all(&self, source: &str, destinations: &[String]) -> Result<()> {
        self.recorder
            .around(|| self.inner.rebase_onto_all(source, destinations))
    }
    fn stale_bookmarks(&self) -> Vec<String> {
        self.inner.stale_bookmarks()
    }
    fn forget_bookmark(&self, name: &str) -> Result<()> {
        self.recorder.around(|| self.inner.forget_bookmark(name))
    }
    fn abandon(&self, revset: &str) -> Result<()> {
        self.recorder.around(|| self.inner.abandon(revset))
    }
    fn merge_into(&self, bookmark: &str, dest: &str) -> Result<()> {
        self.recorder
            .around(|| self.inner.merge_into(bookmark, dest))
    }
    fn is_rooted_in(&self, root: &str, base: &str) -> Result<bool> {
        self.inner.is_rooted_in(root, base)
    }
    fn resolve_change_id(&self, change_id: &str) -> Result<Vec<String>> {
        self.inner.resolve_change_id(change_id)
    }
    fn is_conflicted(&self, revset: &str) -> Result<bool> {
        self.inner.is_conflicted(revset)
    }
    fn first_conflict(&self, revset: &str) -> Result<Option<String>> {
        self.inner.first_conflict(revset)
    }
    fn snapshot(&self) -> Result<()> {
        self.recorder.around(|| self.inner.snapshot())
    }
    fn current_operation_id(&self) -> Result<String> {
        self.inner.current_operation_id()
    }
    fn divergent_change_ids(&self) -> Result<Vec<String>> {
        self.inner.divergent_change_ids()
    }
    fn restore_operation(&self, op_id: &str) -> Result<()> {
        self.recorder.around(|| self.inner.restore_operation(op_id))
    }
}
