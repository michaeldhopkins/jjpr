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
        if self.recorder.finish()
            && let Some(hint) = super::report::after_command(self.recorder.command())
        {
            println!("{hint}");
        }
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
    fn working_copy_parents(&self) -> Result<Vec<String>> {
        self.inner.working_copy_parents()
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
    fn snapshot_if_moved_by(&self, source: &str) -> Result<bool> {
        self.inner.snapshot_if_moved_by(source)
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
    fn restore_operation_keeping_working_copy(&self, op_id: &str) -> Result<()> {
        self.recorder
            .around(|| self.inner.restore_operation_keeping_working_copy(op_id))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    use super::RecordingJj;
    use crate::jj::Jj;
    use crate::jj::types::{Bookmark, GitRemote, LogEntry};
    use crate::undo::journal::Journal;
    use crate::undo::recorder::Recorder;
    use crate::undo::recorder::tests::{FakeRepo, meta};

    /// Answers the snapshot question as told and records restores.
    #[derive(Default)]
    struct Plain {
        moved: bool,
        restored: Mutex<Vec<String>>,
    }

    impl Jj for Plain {
        fn snapshot_if_moved_by(&self, _: &str) -> Result<bool> {
            Ok(self.moved)
        }
        fn restore_operation_keeping_working_copy(&self, op_id: &str) -> Result<()> {
            self.restored.lock().unwrap().push(op_id.to_string());
            Ok(())
        }
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(vec![])
        }
        fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> {
            Ok(vec![])
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
            Ok(vec![])
        }
        fn get_default_branch(&self) -> Result<String> {
            Ok("main".into())
        }
        fn push_bookmark(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok("wc".into())
        }
        fn rebase_onto(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn merge_into(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> {
            Ok(vec![])
        }
        fn is_conflicted(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
    }

    /// Implements only what the trait requires.
    struct Bare;

    impl Jj for Bare {
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(vec![])
        }
        fn get_changes_to_commit(&self, _: &str) -> Result<Vec<LogEntry>> {
            Ok(vec![])
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
            Ok(vec![])
        }
        fn get_default_branch(&self) -> Result<String> {
            Ok("main".into())
        }
        fn push_bookmark(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok("wc".into())
        }
        fn rebase_onto(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn merge_into(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn resolve_change_id(&self, _: &str) -> Result<Vec<String>> {
            Ok(vec![])
        }
        fn is_conflicted(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
    }

    fn recording(inner: Plain) -> (tempfile::TempDir, RecordingJj<Plain>) {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recorder::start(
            Journal::at(dir.path().to_path_buf()),
            Box::new(Arc::new(FakeRepo::at("op"))),
            meta(),
        );
        (dir, RecordingJj::new(inner, rec))
    }

    #[test]
    fn a_stub_claims_no_snapshot_by_default() {
        assert!(!Bare.snapshot_if_moved_by("ch_next").unwrap());
    }

    #[test]
    fn the_snapshot_answer_comes_from_the_wrapped_jj() {
        for moved in [true, false] {
            let (_dir, jj) = recording(Plain {
                moved,
                ..Default::default()
            });
            assert_eq!(jj.snapshot_if_moved_by("ch_next").unwrap(), moved);
        }
    }

    #[test]
    fn a_restore_keeping_the_working_copy_reaches_the_wrapped_jj() {
        let (_dir, jj) = recording(Plain::default());
        jj.restore_operation_keeping_working_copy("op_before")
            .unwrap();
        assert_eq!(*jj.inner.restored.lock().unwrap(), vec!["op_before"]);
    }

    fn fn_names(source: &str) -> BTreeSet<&str> {
        source
            .split("fn ")
            .skip(1)
            .filter_map(|rest| rest.split(['(', '<']).next())
            .filter(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .collect()
    }

    /// A trait method left to its default here bypasses `inner`, so a recorded
    /// command would silently call the stub default instead of jj.
    #[test]
    fn every_jj_method_is_forwarded() {
        let trait_methods = fn_names(include_str!("../jj/mod.rs"));
        let forwarded = fn_names(include_str!("recording_jj.rs"));
        assert!(trait_methods.len() > 20, "found {trait_methods:?}");
        let missing: Vec<_> = trait_methods.difference(&forwarded).collect();
        assert!(missing.is_empty(), "not forwarded: {missing:?}");
    }
}
