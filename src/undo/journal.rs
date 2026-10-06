//! The undo journal: one JSON file per recorded jjpr command.
//!
//! It lives in the repo's own `.jj/repo/jjpr/undo/`, which every workspace of
//! a repo shares, because the operation log it points into is shared too. jj
//! never reads that directory, and it is outside the working copy, so nothing
//! here is ever snapshotted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::forge::ForgeKind;

/// Bumped when an entry's shape changes. An entry with any other number is
/// left alone: undo refuses it rather than guessing at its meaning.
pub const SCHEMA: u32 = 1;

/// One change jjpr made that undo knows how to take back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// `before` is what the remote had (`None`: no such branch), `after` what
    /// jjpr pushed. `pr` is the open PR on the branch at the time, if any.
    Push {
        bookmark: String,
        remote: String,
        before: Option<String>,
        after: String,
        pr: Option<u64>,
    },
    CreatePr {
        number: u64,
        head: String,
    },
    Base {
        number: u64,
        before: String,
        after: String,
    },
    CommentCreate {
        pr: u64,
        id: u64,
        body: String,
    },
    CommentUpdate {
        pr: u64,
        id: u64,
        before: String,
        after: String,
    },
    CommentDelete {
        pr: u64,
        id: u64,
        body: String,
    },
    Body {
        number: u64,
        before: String,
        after: String,
    },
    Ready {
        number: u64,
    },
    Reviewers {
        number: u64,
        added: Vec<String>,
    },
    Merge {
        number: u64,
    },
}

/// An action as recorded: written before the forge call (`confirmed: false`)
/// and confirmed after it, so a jjpr that dies halfway still leaves a record
/// of what may have reached the forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub action: Action,
    pub confirmed: bool,
    /// Taken back by an undo (and not yet redone).
    #[serde(default)]
    pub undone: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The command is still running, or stopped without finishing its record.
    Running,
    Done,
    Undone,
    /// Undone except for what needs `--force`: closing the PRs it opened. A
    /// plain `jjpr undo` moves on to the entry before it.
    KeptOpen,
    /// An undo or redo stopped partway; running it again finishes it.
    PartlyUndone,
}

impl State {
    /// Whether undo took this entry back, wholly or all but its open PRs.
    pub fn undone(self) -> bool {
        matches!(self, Self::Undone | Self::KeptOpen | Self::PartlyUndone)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub schema: u32,
    /// The file name without `.json`. Sorts in the order the commands began.
    #[serde(skip)]
    pub id: String,
    /// `submit`, `merge` or `watch`.
    pub command: String,
    /// Unix seconds.
    pub started_at: u64,
    pub remote: String,
    pub forge: ForgeKind,
    pub owner: String,
    pub repo: String,
    /// The operation the command started from: after submit's snapshot and
    /// fetch, so undo never rolls those back.
    pub start_op: String,
    pub end_op: Option<String>,
    /// [`super::repo::UndoRepo::view_fingerprint`] when the command ended.
    pub end_view: Option<String>,
    /// Operations inside the command's span that jjpr did not make itself
    /// (a working-copy snapshot, a concurrent writer reconciled). Undoing such
    /// an entry would discard them, so it is refused.
    #[serde(default)]
    pub absorbed: Vec<String>,
    pub state: State,
    /// Whether the local repo has been put back (by an undo not yet redone).
    #[serde(default)]
    pub local_undone: bool,
    /// The fingerprint after the last undo, which a redo must still find.
    #[serde(default)]
    pub undone_view: Option<String>,
    /// The newest operation after the last undo or redo, to say what changed
    /// since when the repo no longer matches.
    #[serde(default)]
    pub last_op: Option<String>,
    /// PRs a push of this command closed (the forge does when nothing is
    /// left to merge), which undo reopens.
    #[serde(default)]
    pub closed_by_push: Vec<u64>,
    /// Writes jjpr could not record, for want of the value they replaced.
    /// Undo leaves them and says so.
    #[serde(default)]
    pub missed: Vec<String>,
    pub actions: Vec<Record>,
}

impl Entry {
    /// Whether the entry landed a merge: the one thing undo never takes back.
    pub fn merged(&self) -> Option<u64> {
        self.actions.iter().find_map(|r| match r.action {
            Action::Merge { number } => Some(number),
            _ => None,
        })
    }
}

/// What [`Journal::load`] found.
#[derive(Debug, Default)]
pub struct Loaded {
    /// Oldest first.
    pub entries: Vec<Entry>,
    /// File names that did not parse, or carry an unknown schema.
    pub unreadable: Vec<String>,
}

pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The journal of the repo whose workspace root is `repo_root`.
    pub fn for_repo(repo_root: &Path) -> Result<Self> {
        Ok(Self::at(
            repo_store_dir(repo_root)?.join("jjpr").join("undo"),
        ))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn load(&self) -> Result<Loaded> {
        let mut loaded = Loaded::default();
        let read = match std::fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(loaded),
            Err(e) => return Err(e).context("failed to read the undo journal"),
        };
        for file in read.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            let parsed = std::fs::read_to_string(file.path())
                .ok()
                .and_then(|text| serde_json::from_str::<Entry>(&text).ok())
                .filter(|e| e.schema == SCHEMA);
            match parsed {
                Some(mut entry) => {
                    entry.id = id.to_string();
                    loaded.entries.push(entry);
                }
                None => loaded.unreadable.push(name),
            }
        }
        loaded.entries.sort_by(|a, b| a.id.cmp(&b.id));
        loaded.unreadable.sort();
        Ok(loaded)
    }

    /// Write `entry` whole, through a temporary file, so a reader never sees
    /// half of one.
    pub fn save(&self, entry: &Entry) -> Result<()> {
        std::fs::create_dir_all(&self.dir).context("failed to create the undo journal")?;
        let path = self.dir.join(format!("{}.json", entry.id));
        let tmp = self.dir.join(format!("{}.json.tmp", entry.id));
        std::fs::write(&tmp, serde_json::to_string_pretty(entry)?)
            .context("failed to write the undo journal")?;
        std::fs::rename(&tmp, &path).context("failed to write the undo journal")?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.dir.join(format!("{id}.json"))) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).context("failed to remove an undo journal entry")
            }
            _ => Ok(()),
        }
    }

    /// Remove every entry that began before `id`: history behind something
    /// that cannot be undone is history nothing can reach.
    pub fn prune_before(&self, id: &str) -> Result<()> {
        for entry in self.load()?.entries {
            if entry.id.as_str() < id {
                self.remove(&entry.id)?;
            }
        }
        Ok(())
    }

    /// A new command makes the undone ones impossible to redo: remove them.
    pub fn drop_redo(&self, except: &str) -> Result<()> {
        for entry in self.load()?.entries {
            if entry.id != except && entry.state.undone() {
                self.remove(&entry.id)?;
            }
        }
        Ok(())
    }

    /// Undo or redo posted comment `old` again as `new`: every entry that
    /// names it must follow, or a later undo looks for a comment that is gone.
    pub fn rewrite_comment(&self, pr: u64, old: u64, new: u64) -> Result<()> {
        for mut entry in self.load()?.entries {
            let mut changed = false;
            for record in &mut entry.actions {
                if let Action::CommentCreate { pr: p, id, .. }
                | Action::CommentUpdate { pr: p, id, .. }
                | Action::CommentDelete { pr: p, id, .. } = &mut record.action
                    && *p == pr
                    && *id == old
                {
                    *id = new;
                    changed = true;
                }
            }
            if changed {
                self.save(&entry)?;
            }
        }
        Ok(())
    }
}

pub use crate::heartbeat::alive;
pub use lock::Lock;

mod lock;

/// An id that sorts by start time: zero-padded nanoseconds, then the pid so two
/// processes starting in the same nanosecond still differ.
pub fn new_id(nanos: u128, pid: u32) -> String {
    format!("{nanos:024}-{pid}")
}

/// `.jj/repo` of the workspace at `repo_root`. In a second workspace that is a
/// file holding the main repo's path, relative to the workspace's `.jj`.
pub fn repo_store_dir(repo_root: &Path) -> Result<PathBuf> {
    let dot_jj = repo_root.join(".jj");
    let repo = dot_jj.join("repo");
    if repo.is_dir() {
        return Ok(repo);
    }
    let pointer = std::fs::read_to_string(&repo)
        .with_context(|| format!("failed to read {}", repo.display()))?;
    let target = Path::new(pointer.trim());
    Ok(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dot_jj.join(target)
    })
}

/// The entry `jjpr undo` acts on: the newest one not wholly undone.
///
/// An entry left with its PRs open counts as undone, unless `force` asks to
/// close them now.
pub fn undo_target(entries: &[Entry], force: bool) -> Option<&Entry> {
    entries.iter().rev().find(|e| match e.state {
        State::Undone => false,
        State::KeptOpen => force,
        _ => true,
    })
}

/// The entry `jjpr redo` acts on: the oldest of the undone entries that end
/// the journal. An entry recorded after an undo ends redo, since
/// [`Journal::drop_redo`] removed what it would have redone.
pub fn redo_target(entries: &[Entry]) -> Option<&Entry> {
    let undone = entries
        .iter()
        .rev()
        .take_while(|e| e.state.undone())
        .count();
    entries.get(entries.len() - undone)
}

/// The pid of the process that recorded entry `id` (see [`new_id`]).
pub fn pid_of(id: &str) -> Option<u32> {
    id.rsplit('-').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, state: State) -> Entry {
        Entry {
            schema: SCHEMA,
            id: id.to_string(),
            command: "submit".to_string(),
            started_at: 0,
            remote: "origin".to_string(),
            forge: ForgeKind::GitHub,
            owner: "o".to_string(),
            repo: "r".to_string(),
            start_op: "s".to_string(),
            end_op: Some("e".to_string()),
            end_view: Some("v".to_string()),
            absorbed: vec![],
            state,
            local_undone: false,
            undone_view: None,
            last_op: None,
            closed_by_push: Vec::new(),
            missed: Vec::new(),
            actions: vec![],
        }
    }

    fn all_actions() -> Vec<Record> {
        let record = |action| Record {
            action,
            confirmed: true,
            undone: false,
        };
        vec![
            record(Action::Push {
                bookmark: "b".into(),
                remote: "origin".into(),
                before: None,
                after: "c1".into(),
                pr: Some(3),
            }),
            record(Action::CreatePr {
                number: 4,
                head: "b".into(),
            }),
            record(Action::Base {
                number: 4,
                before: "main".into(),
                after: "a".into(),
            }),
            record(Action::CommentCreate {
                pr: 4,
                id: 9,
                body: "x".into(),
            }),
            record(Action::CommentUpdate {
                pr: 4,
                id: 9,
                before: "x".into(),
                after: "y".into(),
            }),
            record(Action::CommentDelete {
                pr: 4,
                id: 9,
                body: "y".into(),
            }),
            record(Action::Body {
                number: 4,
                before: "".into(),
                after: "z".into(),
            }),
            record(Action::Ready { number: 4 }),
            record(Action::Reviewers {
                number: 4,
                added: vec!["alice".into()],
            }),
            record(Action::Merge { number: 4 }),
        ]
    }

    #[test]
    fn every_action_survives_a_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().join("undo"));
        let mut e = entry("0001-1", State::Done);
        e.actions = all_actions();
        journal.save(&e).unwrap();
        let loaded = journal.load().unwrap();
        assert_eq!(loaded.entries, vec![e]);
        assert!(loaded.unreadable.is_empty());
    }

    #[test]
    fn a_missing_journal_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = Journal::at(dir.path().join("nope")).load().unwrap();
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn unreadable_files_and_other_schemas_are_reported_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().to_path_buf());
        journal.save(&entry("0002-1", State::Done)).unwrap();
        std::fs::write(dir.path().join("0001-1.json"), "{ not json").unwrap();
        let mut future = entry("0003-1", State::Done);
        future.schema = SCHEMA + 1;
        journal.save(&future).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let loaded = journal.load().unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].id, "0002-1");
        assert_eq!(loaded.unreadable, vec!["0001-1.json", "0003-1.json"]);
    }

    #[test]
    fn entries_load_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().to_path_buf());
        for id in [new_id(30, 1), new_id(4, 9), new_id(200, 2)] {
            journal.save(&entry(&id, State::Done)).unwrap();
        }
        let ids: Vec<_> = journal
            .load()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec![new_id(4, 9), new_id(30, 1), new_id(200, 2)]);
    }

    #[test]
    fn prune_before_keeps_the_barrier_and_what_follows() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().to_path_buf());
        for id in ["1", "2", "3", "4"] {
            journal.save(&entry(id, State::Done)).unwrap();
        }
        journal.prune_before("3").unwrap();
        let ids: Vec<_> = journal
            .load()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec!["3", "4"]);
    }

    #[test]
    fn drop_redo_removes_undone_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().to_path_buf());
        journal.save(&entry("1", State::Done)).unwrap();
        journal.save(&entry("2", State::Undone)).unwrap();
        journal.save(&entry("3", State::PartlyUndone)).unwrap();
        journal.save(&entry("4", State::Running)).unwrap();
        journal.drop_redo("4").unwrap();
        let ids: Vec<_> = journal
            .load()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec!["1", "4"]);
    }

    #[test]
    fn undo_takes_the_newest_entry_not_wholly_undone() {
        let es = vec![
            entry("1", State::Done),
            entry("2", State::Done),
            entry("3", State::Undone),
        ];
        assert_eq!(undo_target(&es, false).unwrap().id, "2");
        let es = vec![entry("1", State::Done), entry("2", State::PartlyUndone)];
        assert_eq!(undo_target(&es, false).unwrap().id, "2", "finish it first");
        assert!(undo_target(&[entry("1", State::Undone)], true).is_none());
        assert!(undo_target(&[], false).is_none());
    }

    #[test]
    fn an_entry_left_with_open_prs_is_passed_over_unless_forced() {
        let es = vec![entry("1", State::Done), entry("2", State::KeptOpen)];
        assert_eq!(undo_target(&es, false).unwrap().id, "1");
        assert_eq!(undo_target(&es, true).unwrap().id, "2");
        assert_eq!(redo_target(&es).unwrap().id, "2");
    }

    #[test]
    fn pid_of_reads_the_id_suffix() {
        assert_eq!(pid_of(&new_id(5, 4242)), Some(4242));
        assert_eq!(pid_of("nonsense"), None);
    }

    #[test]
    fn a_lock_is_exclusive_until_dropped_and_a_dead_holder_s_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().to_path_buf());
        let held = journal.lock().unwrap();
        let err = journal.lock().err().expect("held");
        assert!(
            err.to_string()
                .contains("another jjpr undo or redo is running"),
            "{err}"
        );
        drop(held);
        let again = journal.lock().unwrap();
        drop(again);
        // A pid no live process has (beyond pid_max).
        std::fs::write(dir.path().join("undo.lock"), "99999999").unwrap();
        assert!(journal.lock().is_ok());
    }

    #[test]
    fn a_reposted_comment_is_renamed_in_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::at(dir.path().to_path_buf());
        let mut a = entry("1", State::Done);
        a.actions = vec![Record {
            action: Action::CommentCreate {
                pr: 4,
                id: 9,
                body: "x".into(),
            },
            confirmed: true,
            undone: false,
        }];
        let mut b = entry("2", State::Done);
        b.actions = vec![Record {
            action: Action::CommentUpdate {
                pr: 4,
                id: 9,
                before: "x".into(),
                after: "y".into(),
            },
            confirmed: true,
            undone: false,
        }];
        journal.save(&a).unwrap();
        journal.save(&b).unwrap();
        journal.rewrite_comment(4, 9, 15).unwrap();
        journal.rewrite_comment(5, 15, 99).unwrap();
        let ids: Vec<u64> = journal
            .load()
            .unwrap()
            .entries
            .iter()
            .map(|e| match e.actions[0].action {
                Action::CommentCreate { id, .. } | Action::CommentUpdate { id, .. } => id,
                _ => 0,
            })
            .collect();
        assert_eq!(ids, vec![15, 15], "another PR's comment 15 is not this one");
    }

    #[test]
    fn redo_takes_the_oldest_of_the_trailing_undone_entries() {
        let es = vec![
            entry("1", State::Done),
            entry("2", State::Undone),
            entry("3", State::Undone),
        ];
        assert_eq!(redo_target(&es).unwrap().id, "2");
        let es = vec![entry("1", State::Undone), entry("2", State::PartlyUndone)];
        assert_eq!(redo_target(&es).unwrap().id, "1");
        assert!(redo_target(&[entry("1", State::Done)]).is_none());
        assert!(redo_target(&[]).is_none());
    }

    #[test]
    fn merged_names_the_merged_pr() {
        let mut e = entry("1", State::Done);
        assert_eq!(e.merged(), None);
        e.actions = all_actions();
        assert_eq!(e.merged(), Some(4));
    }

    #[test]
    fn the_store_of_a_second_workspace_is_the_main_repo_s() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(main.join(".jj/repo")).unwrap();
        assert_eq!(repo_store_dir(&main).unwrap(), main.join(".jj/repo"));
        let second = dir.path().join("second");
        std::fs::create_dir_all(second.join(".jj")).unwrap();
        std::fs::write(second.join(".jj/repo"), "../../main/.jj/repo").unwrap();
        let store = repo_store_dir(&second).unwrap();
        assert_eq!(
            store.canonicalize().unwrap(),
            main.join(".jj/repo").canonicalize().unwrap()
        );
    }
}
