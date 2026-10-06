//! Records what one jjpr command does, for `jjpr undo`.
//!
//! [`super::RecordingJj`] and [`super::RecordingForge`] feed it: the pushes,
//! and every forge write with the value it replaced. The replaced values come
//! from the reads jjpr makes anyway (the open PR list, each PR's comments), so
//! recording costs no extra forge requests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::forge::ForgeKind;
use crate::forge::types::{IssueComment, PullRequest};

use super::journal::{Action, Entry, Journal, Record, SCHEMA, State, new_id};
use super::repo::UndoRepo;

mod around;

/// Where a command's changes go.
#[derive(Debug, Clone)]
pub struct Meta {
    pub command: String,
    pub remote: String,
    pub forge: ForgeKind,
    pub owner: String,
    pub repo: String,
}

/// What jjpr last saw of a PR, kept current as it writes.
#[derive(Debug, Clone, Default)]
pub struct KnownPr {
    pub head: String,
    pub base: String,
    pub body: String,
    pub reviewers: Vec<String>,
    /// Open when jjpr read it.
    pub open: bool,
}

#[derive(Default)]
struct Inner {
    entry: Option<Entry>,
    written: bool,
    /// The operation jjpr's own last jj command left. Operations after it,
    /// until jjpr's next command, are someone else's.
    last_own: String,
    /// Whether jjpr changed the repo in this entry, a fetch aside.
    worked: bool,
    prs: HashMap<u64, KnownPr>,
    /// Comment id to its PR and body.
    comments: HashMap<u64, (u64, String)>,
}

pub struct Recorder {
    journal: Journal,
    repo: Box<dyn UndoRepo>,
    meta: Meta,
    inner: Mutex<Inner>,
    /// A journal write has failed and been reported once.
    warned: AtomicBool,
}

impl Recorder {
    /// Start recording a command, from the repo's current operation.
    pub fn start(journal: Journal, repo: Box<dyn UndoRepo>, meta: Meta) -> Arc<Self> {
        let recorder = Arc::new(Self {
            journal,
            repo,
            meta,
            inner: Mutex::new(Inner::default()),
            warned: AtomicBool::new(false),
        });
        recorder.begin();
        recorder
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn begin(&self) {
        let start_op = match self.repo.current_op() {
            Ok(op) => op,
            Err(e) => {
                eprintln!("  Warning: jjpr undo cannot record this command: {e}");
                return;
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mut inner = self.lock();
        inner.written = false;
        inner.last_own = start_op.clone();
        inner.worked = false;
        inner.entry = Some(Entry {
            schema: SCHEMA,
            id: new_id(now.as_nanos(), std::process::id()),
            command: self.meta.command.clone(),
            started_at: now.as_secs(),
            remote: self.meta.remote.clone(),
            forge: self.meta.forge,
            owner: self.meta.owner.clone(),
            repo: self.meta.repo.clone(),
            start_op,
            end_op: None,
            end_view: None,
            absorbed: Vec::new(),
            state: State::Running,
            local_undone: false,
            undone_view: None,
            last_op: None,
            closed_by_push: Vec::new(),
            missed: Vec::new(),
            actions: Vec::new(),
        });
    }

    /// Close the current entry and open the next. `jjpr watch` calls this at
    /// the top of each poll, so each poll that changes anything is its own
    /// entry.
    pub fn checkpoint(&self) {
        self.finish();
        self.begin();
    }

    /// Close the current entry: record where the command ended, and drop it
    /// if it changed nothing.
    ///
    /// The entry ends at jjpr's own last operation, not at whatever the repo
    /// is at now: anything run after it (in `watch`'s sleep, say) is the
    /// user's, and the fingerprint check at undo time protects it.
    pub fn finish(&self) {
        let (entry, last_own, written, worked) = {
            let mut inner = self.lock();
            (
                inner.entry.take(),
                inner.last_own.clone(),
                inner.written,
                inner.worked,
            )
        };
        let Some(mut entry) = entry else {
            return;
        };
        let nothing = entry.actions.is_empty() && entry.missed.is_empty();
        if nothing && !worked {
            if written && let Err(e) = self.journal.remove(&entry.id) {
                warn(&self.warned, &e);
            }
            return;
        }
        entry.end_view = self.repo.view_fingerprint_at(&last_own).ok();
        entry.end_op = Some(last_own);
        entry.state = State::Done;
        let mut inner = self.lock();
        self.save(&mut inner, &entry);
        if (entry.merged().is_some() || self.history_unreachable(&entry))
            && let Err(e) = self.journal.prune_before(&entry.id)
        {
            warn(&self.warned, &e);
        }
    }

    /// Whether the repo changed between the previous recorded command and
    /// this one (the user amended a commit, say). Undoing this one then cannot
    /// bring back the state the previous one left, so nothing older can ever
    /// be undone again.
    fn history_unreachable(&self, entry: &Entry) -> bool {
        let Ok(loaded) = self.journal.load() else {
            return false;
        };
        let previous = loaded
            .entries
            .iter()
            .rev()
            .find(|e| e.id < entry.id && e.state == State::Done);
        let Some(previous) = previous else {
            return false;
        };
        match self.repo.view_fingerprint_at(&entry.start_op) {
            Ok(start) => previous.end_view.as_deref() != Some(start.as_str()),
            Err(_) => false,
        }
    }

    fn save(&self, inner: &mut Inner, entry: &Entry) {
        let first = !inner.written;
        if let Err(e) = self.journal.save(entry) {
            warn(&self.warned, &e);
            return;
        }
        inner.written = true;
        if first && let Err(e) = self.journal.drop_redo(&entry.id) {
            warn(&self.warned, &e);
        }
    }

    /// A write went through that jjpr could not record, for want of the value
    /// it replaced. Undo says it leaves it.
    pub fn missed(&self, what: String) {
        let mut inner = self.lock();
        if let Some(mut entry) = inner.entry.take() {
            entry.missed.push(what);
            self.save(&mut inner, &entry);
            inner.entry = Some(entry);
        }
    }

    /// A PR jjpr read as open, and then found closed (as a push closes a PR
    /// with nothing left to merge): undo reopens it.
    pub fn note_closed(&self, number: u64) {
        let mut inner = self.lock();
        let open_before = inner.prs.get(&number).is_some_and(|p| p.open);
        if let Some(mut entry) = inner.entry.take() {
            let pushed = entry
                .actions
                .iter()
                .any(|r| matches!(r.action, Action::Push { pr: Some(n), .. } if n == number));
            if open_before && pushed && !entry.closed_by_push.contains(&number) {
                entry.closed_by_push.push(number);
                self.save(&mut inner, &entry);
            }
            inner.entry = Some(entry);
        }
    }

    /// Record `action` before jjpr attempts it. Returns its index for
    /// [`Recorder::confirm`] or [`Recorder::retract`].
    pub fn intent(&self, action: Action) -> Option<usize> {
        let mut inner = self.lock();
        let mut entry = inner.entry.take()?;
        entry.actions.push(Record {
            action,
            confirmed: false,
            undone: false,
        });
        let index = entry.actions.len() - 1;
        self.save(&mut inner, &entry);
        inner.entry = Some(entry);
        Some(index)
    }

    /// The attempt succeeded. `action`, when given, replaces the intent (a
    /// created PR's number is known only now).
    pub fn confirm(&self, index: Option<usize>, action: Option<Action>) {
        self.update(index, |record| {
            record.confirmed = true;
            if let Some(action) = action {
                record.action = action;
            }
        });
    }

    /// The attempt failed: the forge refused it, so there is nothing to undo.
    pub fn retract(&self, index: Option<usize>) {
        let mut inner = self.lock();
        let (Some(index), Some(mut entry)) = (index, inner.entry.take()) else {
            return;
        };
        if index < entry.actions.len() {
            entry.actions.remove(index);
            self.save(&mut inner, &entry);
        }
        inner.entry = Some(entry);
    }

    fn update(&self, index: Option<usize>, change: impl FnOnce(&mut Record)) {
        let mut inner = self.lock();
        let (Some(index), Some(mut entry)) = (index, inner.entry.take()) else {
            return;
        };
        if let Some(record) = entry.actions.get_mut(index) {
            change(record);
            self.save(&mut inner, &entry);
        }
        inner.entry = Some(entry);
    }

    pub fn repo(&self) -> &dyn UndoRepo {
        self.repo.as_ref()
    }

    pub fn forge(&self) -> ForgeKind {
        self.meta.forge
    }

    /// Remember PRs jjpr has read, for the values its writes replace.
    pub fn note_prs<'a>(&self, prs: impl IntoIterator<Item = &'a PullRequest>, open: bool) {
        let mut inner = self.lock();
        // A fork's PR can share a branch name with ours; jjpr never writes to
        // one, so it is left out (the same rule as `build_pr_map`).
        let ours = format!("{}:", self.meta.owner);
        for pr in prs {
            let label = &pr.head.label;
            if label.contains(':') && !label.starts_with(&ours) {
                continue;
            }
            inner.prs.insert(
                pr.number,
                KnownPr {
                    head: pr.head.ref_name.clone(),
                    base: pr.base.ref_name.clone(),
                    body: pr.body.clone().unwrap_or_default(),
                    reviewers: pr.requested_reviewers.clone(),
                    open,
                },
            );
        }
    }

    pub fn known_pr(&self, number: u64) -> Option<KnownPr> {
        self.lock().prs.get(&number).cloned()
    }

    /// The open PR whose branch is `head`, as jjpr last read it.
    pub fn open_pr_for(&self, head: &str) -> Option<u64> {
        let inner = self.lock();
        inner
            .prs
            .iter()
            .find(|(_, pr)| pr.open && pr.head == head)
            .map(|(n, _)| *n)
    }

    pub fn update_known_pr(&self, number: u64, change: impl FnOnce(&mut KnownPr)) {
        if let Some(pr) = self.lock().prs.get_mut(&number) {
            change(pr);
        }
    }

    pub fn note_comments(&self, pr: u64, comments: &[IssueComment]) {
        let mut inner = self.lock();
        for c in comments {
            inner
                .comments
                .insert(c.id, (pr, c.body.clone().unwrap_or_default()));
        }
    }

    pub fn known_comment(&self, id: u64) -> Option<(u64, String)> {
        self.lock().comments.get(&id).cloned()
    }

    pub fn set_known_comment(&self, id: u64, pr: u64, body: Option<String>) {
        let mut inner = self.lock();
        match body {
            Some(body) => inner.comments.insert(id, (pr, body)),
            None => inner.comments.remove(&id),
        };
    }

    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<Entry> {
        self.lock().entry.clone()
    }
}

fn warn(warned: &AtomicBool, e: &anyhow::Error) {
    if !warned.swap(true, Ordering::Relaxed) {
        eprintln!("  Warning: could not write the undo journal; jjpr undo may not work: {e}");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::undo::repo::{Operation, Targets};
    use anyhow::Result;

    /// An in-memory repo whose operation id and fingerprint the test sets.
    #[derive(Default)]
    pub(crate) struct FakeRepo {
        pub op: Mutex<String>,
        pub view: Mutex<String>,
        /// The fingerprint at a past operation; the current one when unset.
        pub view_at: Mutex<Option<String>>,
        pub since: Mutex<Vec<Operation>>,
    }

    impl FakeRepo {
        pub fn at(op: &str) -> Self {
            let repo = Self::default();
            *repo.op.lock().unwrap() = op.to_string();
            repo
        }
    }

    impl UndoRepo for Arc<FakeRepo> {
        fn current_op(&self) -> Result<String> {
            Ok(self.op.lock().unwrap().clone())
        }
        fn snapshot(&self) -> Result<()> {
            Ok(())
        }
        fn view_fingerprint(&self) -> Result<String> {
            Ok(self.view.lock().unwrap().clone())
        }
        fn view_fingerprint_at(&self, _: &str) -> Result<String> {
            let at = self.view_at.lock().unwrap().clone();
            Ok(at.unwrap_or_else(|| self.view.lock().unwrap().clone()))
        }
        fn ops_since(&self, _: &str, _: usize) -> Result<Option<Vec<Operation>>> {
            Ok(Some(self.since.lock().unwrap().clone()))
        }
        fn op_exists(&self, _: &str) -> Result<bool> {
            Ok(true)
        }
        fn restore_repo_only(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn targets(&self, _: &str, _: &str) -> Result<Targets> {
            Ok(Targets::default())
        }
        fn set_bookmark(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn delete_bookmark(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn push_bookmark(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }

    pub(crate) fn meta() -> Meta {
        Meta {
            command: "submit".into(),
            remote: "origin".into(),
            forge: ForgeKind::GitHub,
            owner: "o".into(),
            repo: "r".into(),
        }
    }

    fn setup() -> (tempfile::TempDir, Arc<FakeRepo>, Arc<Recorder>) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Arc::new(FakeRepo::at("op1"));
        let rec = Recorder::start(
            Journal::at(dir.path().to_path_buf()),
            Box::new(repo.clone()),
            meta(),
        );
        (dir, repo, rec)
    }

    fn entries(dir: &tempfile::TempDir) -> Vec<Entry> {
        Journal::at(dir.path().to_path_buf())
            .load()
            .unwrap()
            .entries
    }

    #[test]
    fn a_command_that_changed_nothing_leaves_no_entry() {
        let (dir, _repo, rec) = setup();
        rec.finish();
        assert!(entries(&dir).is_empty());
    }

    #[test]
    fn local_changes_alone_are_recorded() {
        let (dir, repo, rec) = setup();
        *repo.view.lock().unwrap() = "view".into();
        rec.around(|| {
            *repo.op.lock().unwrap() = "op2".into();
            Ok(())
        })
        .unwrap();
        rec.finish();
        let es = entries(&dir);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].start_op, "op1");
        assert_eq!(es[0].end_op.as_deref(), Some("op2"));
        assert_eq!(es[0].end_view.as_deref(), Some("view"));
        assert_eq!(es[0].state, State::Done);
        assert!(es[0].absorbed.is_empty());
    }

    #[test]
    fn someone_else_s_operations_after_jjpr_s_last_are_not_the_entry_s() {
        let (dir, repo, rec) = setup();
        rec.intent(Action::Ready { number: 1 });
        *repo.op.lock().unwrap() = "user's".into();
        rec.finish();
        let es = entries(&dir);
        assert_eq!(
            es[0].end_op.as_deref(),
            Some("op1"),
            "jjpr made no operation"
        );
    }

    #[test]
    fn an_intent_is_on_disk_before_it_is_confirmed() {
        let (dir, _repo, rec) = setup();
        let i = rec.intent(Action::Ready { number: 3 });
        let es = entries(&dir);
        assert_eq!(es[0].state, State::Running);
        assert!(!es[0].actions[0].confirmed);
        rec.confirm(i, None);
        assert!(entries(&dir)[0].actions[0].confirmed);
    }

    #[test]
    fn confirm_can_replace_the_action_and_retract_removes_it() {
        let (dir, _repo, rec) = setup();
        let a = rec.intent(Action::CreatePr {
            number: 0,
            head: "b".into(),
        });
        let b = rec.intent(Action::Ready { number: 9 });
        rec.confirm(
            a,
            Some(Action::CreatePr {
                number: 7,
                head: "b".into(),
            }),
        );
        rec.retract(b);
        let es = entries(&dir);
        assert_eq!(es[0].actions.len(), 1);
        assert_eq!(
            es[0].actions[0].action,
            Action::CreatePr {
                number: 7,
                head: "b".into()
            }
        );
    }

    #[test]
    fn foreign_operations_inside_the_span_are_named() {
        let (dir, repo, rec) = setup();
        rec.intent(Action::Ready { number: 1 });
        // Someone's `jj describe` lands between two of jjpr's commands, and
        // jjpr's own command takes a working-copy snapshot.
        *repo.op.lock().unwrap() = "op3".into();
        *repo.since.lock().unwrap() = vec![
            Operation {
                id: "op5".into(),
                description: "push bookmark b to git remote origin".into(),
            },
            Operation {
                id: "op4".into(),
                description: "snapshot working copy".into(),
            },
            Operation {
                id: "op3".into(),
                description: "describe commit 1234".into(),
            },
        ];
        rec.around(|| {
            *repo.op.lock().unwrap() = "op5".into();
            Ok(())
        })
        .unwrap();
        rec.finish();
        let absorbed = &entries(&dir)[0].absorbed;
        assert!(
            absorbed.contains(&"describe commit 1234".to_string()),
            "{absorbed:?}"
        );
        assert!(
            absorbed.contains(&"snapshot working copy".to_string()),
            "{absorbed:?}"
        );
    }

    #[test]
    fn a_change_between_commands_prunes_the_older_ones() {
        let (dir, repo, rec) = setup();
        *repo.view.lock().unwrap() = "after first".into();
        rec.intent(Action::Ready { number: 1 });
        rec.checkpoint();
        rec.intent(Action::Ready { number: 2 });
        *repo.view_at.lock().unwrap() = Some("after first".into());
        rec.checkpoint();
        assert_eq!(entries(&dir).len(), 2, "nothing happened in between");
        rec.intent(Action::Ready { number: 3 });
        *repo.view_at.lock().unwrap() = Some("the user amended a commit".into());
        rec.finish();
        let es = entries(&dir);
        assert_eq!(es.len(), 1, "{es:?}");
        assert_eq!(es[0].actions[0].action, Action::Ready { number: 3 });
    }

    #[test]
    fn a_poll_that_only_fetched_leaves_no_entry() {
        let (dir, repo, rec) = setup();
        rec.around_fetch(|| {
            *repo.op.lock().unwrap() = "fetched".into();
            Ok(())
        })
        .unwrap();
        rec.checkpoint();
        assert!(entries(&dir).is_empty());
        assert_eq!(rec.current().unwrap().start_op, "fetched");
    }

    fn pr(number: u64, head: &str, label: &str) -> PullRequest {
        let side = |r: &str, l: &str| crate::forge::types::PullRequestRef {
            ref_name: r.into(),
            label: l.into(),
            sha: String::new(),
        };
        PullRequest {
            number,
            html_url: String::new(),
            title: String::new(),
            body: None,
            base: side("main", ""),
            head: side(head, label),
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
            author: String::new(),
            stack: None,
        }
    }

    #[test]
    fn a_fork_s_pr_on_the_same_branch_name_is_not_ours() {
        let (_dir, _repo, rec) = setup();
        rec.note_prs([&pr(7, "feat", "someone:feat")], true);
        assert_eq!(rec.open_pr_for("feat"), None);
        rec.note_prs([&pr(8, "feat", "o:feat")], true);
        assert_eq!(rec.open_pr_for("feat"), Some(8));
    }

    #[test]
    fn only_a_pr_read_open_and_pushed_to_counts_as_closed_by_the_push() {
        let (_dir, _repo, rec) = setup();
        rec.note_prs([&pr(3, "a", "")], false);
        rec.note_prs([&pr(4, "b", "")], true);
        for (n, b) in [(3, "a"), (4, "b")] {
            rec.intent(Action::Push {
                bookmark: b.into(),
                remote: "origin".into(),
                before: None,
                after: "c".into(),
                pr: Some(n),
            });
        }
        rec.note_closed(3);
        rec.note_closed(4);
        rec.note_closed(5);
        assert_eq!(rec.current().unwrap().closed_by_push, vec![4]);
    }

    #[test]
    fn checkpoint_splits_entries() {
        let (dir, repo, rec) = setup();
        rec.intent(Action::Ready { number: 1 });
        *repo.op.lock().unwrap() = "op2".into();
        rec.checkpoint();
        rec.intent(Action::Ready { number: 2 });
        rec.finish();
        let es = entries(&dir);
        assert_eq!(es.len(), 2);
        assert_eq!(es[1].start_op, "op2");
    }

    #[test]
    fn a_merge_prunes_older_history_and_a_new_entry_ends_redo() {
        let (dir, repo, _rec) = setup();
        let journal = Journal::at(dir.path().to_path_buf());
        let mut old = Entry {
            id: "0".into(),
            state: State::Done,
            ..entry_like()
        };
        journal.save(&old).unwrap();
        old.id = "1".into();
        old.state = State::Undone;
        journal.save(&old).unwrap();
        let rec = Recorder::start(
            Journal::at(dir.path().to_path_buf()),
            Box::new(repo.clone()),
            meta(),
        );
        rec.intent(Action::Merge { number: 5 });
        rec.finish();
        let es = entries(&dir);
        assert_eq!(es.len(), 1, "{es:?}");
        assert_eq!(es[0].merged(), Some(5));
    }

    fn entry_like() -> Entry {
        Entry {
            schema: SCHEMA,
            id: String::new(),
            command: "submit".into(),
            started_at: 0,
            remote: "origin".into(),
            forge: ForgeKind::GitHub,
            owner: "o".into(),
            repo: "r".into(),
            start_op: "s".into(),
            end_op: None,
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

    #[test]
    fn the_cache_tracks_prs_and_comments() {
        let (_dir, _repo, rec) = setup();
        rec.note_comments(
            4,
            &[IssueComment {
                id: 8,
                body: Some("x".into()),
            }],
        );
        assert_eq!(rec.known_comment(8), Some((4, "x".into())));
        rec.set_known_comment(8, 4, None);
        assert_eq!(rec.known_comment(8), None);
        assert!(rec.known_pr(4).is_none());
        assert!(rec.open_pr_for("b").is_none());
    }
}
