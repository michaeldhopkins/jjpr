//! The operation a reconcile rolls a mangling rebase back to.

use crate::config::ReconcileStrategy;
use crate::jj::Jj;
use crate::jj::types::NarrowedSegment;

/// The revset whose descendants the reconcile's own jj commands snapshot when
/// `@` is among them: the rebase's root, or for a merge sync each remaining
/// bookmark, since `merge_into` snapshots only when `@` is on the bookmark it
/// advances. `None` when the reconcile rewrites nothing.
pub(super) fn snapshot_scope(
    strategy: ReconcileStrategy,
    remaining: &[NarrowedSegment],
    moved_root: Option<&str>,
) -> Option<String> {
    let root = moved_root?;
    match strategy {
        ReconcileStrategy::Rebase => Some(root.to_string()),
        ReconcileStrategy::Merge => Some(
            remaining
                .iter()
                .map(|s| s.bookmark.name.as_str())
                .collect::<Vec<_>>()
                .join(" | "),
        )
        .filter(|s| !s.is_empty()),
    }
}

/// The operation right after fetch: the clean, work-preserving point to roll a
/// bad rebase back to. Nothing rolls back past it: rolling back to before the
/// fetch would discard a concurrent process's work, and jj already preserves
/// both sides' commits in the reconciled state.
///
/// A rebase or merge sync that reaches `@` snapshots the user's edits first.
/// Unless they are recorded before this point, restoring to it rolls them off
/// the disk, so `scope` (see [`snapshot_scope`]) is snapshotted before the
/// operation is read.
/// `None` when the operation id cannot be read; the caller then has nothing to
/// restore to and says so.
pub(super) fn capture(jj: &dyn Jj, scope: Option<&str>) -> Result<Option<RestorePoint>, String> {
    let edits_recorded = match scope {
        Some(scope) => jj
            .snapshot_if_moved_by(scope)
            .map_err(|e| format!("Failed to snapshot the working copy: {e}"))?,
        None => false,
    };
    let op = jj.current_operation_id().ok().filter(|s| !s.is_empty());
    Ok(op.map(|op| RestorePoint { op, edits_recorded }))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RestorePoint {
    op: String,
    /// Whether `@` was in the snapshot scope, so its edits are recorded at `op`.
    edits_recorded: bool,
}

impl RestorePoint {
    /// Roll the repo back to the captured operation. When `@` was outside the
    /// snapshot scope nothing was snapshotted, and a restore that touched the
    /// working copy would snapshot the edits itself and then check out the older
    /// `@` without them; leaving the working copy alone keeps them, since `@` is
    /// the same commit at both operations.
    pub(super) fn restore(&self, jj: &dyn Jj) -> anyhow::Result<()> {
        if self.edits_recorded {
            jj.restore_operation(&self.op)
        } else {
            jj.restore_operation_keeping_working_copy(&self.op)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;

    use super::{RestorePoint, capture, snapshot_scope};
    use crate::config::ReconcileStrategy;
    use crate::jj::Jj;
    use crate::jj::types::{Bookmark, GitRemote, LogEntry, NarrowedSegment};

    #[derive(Default)]
    struct OrderJj {
        calls: Mutex<Vec<String>>,
        op: &'static str,
        snapshot_error: Option<&'static str>,
    }
    impl OrderJj {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("poisoned").clone()
        }
    }
    impl Jj for OrderJj {
        fn snapshot_if_moved_by(&self, source: &str) -> Result<bool> {
            if let Some(e) = self.snapshot_error {
                anyhow::bail!("{e}");
            }
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("snapshot {source}"));
            Ok(true)
        }
        fn current_operation_id(&self) -> Result<String> {
            self.calls.lock().expect("poisoned").push("op".into());
            Ok(self.op.to_string())
        }
        fn restore_operation(&self, op: &str) -> Result<()> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("restore {op}"));
            Ok(())
        }
        fn restore_operation_keeping_working_copy(&self, op: &str) -> Result<()> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("restore keeping {op}"));
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

    fn point(op: &str, edits_recorded: bool) -> RestorePoint {
        RestorePoint {
            op: op.to_string(),
            edits_recorded,
        }
    }

    #[test]
    fn snapshots_the_moved_root_before_reading_the_operation() {
        let jj = OrderJj {
            op: "pf",
            ..Default::default()
        };
        assert_eq!(capture(&jj, Some("ch_next")), Ok(Some(point("pf", true))));
        assert_eq!(jj.calls(), vec!["snapshot ch_next", "op"]);
    }

    #[test]
    fn reads_the_operation_alone_when_nothing_will_move() {
        let jj = OrderJj {
            op: "pf",
            ..Default::default()
        };
        assert_eq!(capture(&jj, None), Ok(Some(point("pf", false))));
        assert_eq!(jj.calls(), vec!["op"]);
    }

    #[test]
    fn an_empty_operation_id_is_no_restore_point() {
        let jj = OrderJj::default();
        assert_eq!(capture(&jj, Some("ch_next")), Ok(None));
    }

    #[test]
    fn a_failed_snapshot_is_an_error_and_reads_no_operation() {
        let jj = OrderJj {
            snapshot_error: Some("locked"),
            ..Default::default()
        };
        let err = capture(&jj, Some("ch_next")).unwrap_err();
        assert!(
            err.contains("snapshot") && err.contains("locked"),
            "got {err}"
        );
        assert!(jj.calls().is_empty());
    }

    fn segment(name: &str) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.into(),
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![],
            merge_source_names: vec![],
        }
    }

    #[test]
    fn a_rebase_scopes_the_snapshot_to_its_root() {
        let remaining = [segment("a"), segment("b")];
        let scope = snapshot_scope(ReconcileStrategy::Rebase, &remaining, Some("ch_low"));
        assert_eq!(scope.as_deref(), Some("ch_low"));
    }

    /// `merge_into` snapshots only when `@` is on a bookmark it advances, so a
    /// checkout lower in a segment is left alone here too.
    #[test]
    fn a_merge_sync_scopes_the_snapshot_to_the_bookmarks_it_advances() {
        let remaining = [segment("a"), segment("b")];
        let scope = snapshot_scope(ReconcileStrategy::Merge, &remaining, Some("ch_low"));
        assert_eq!(scope.as_deref(), Some("a | b"));
    }

    #[test]
    fn nothing_rewritten_means_no_snapshot_scope() {
        let remaining = [segment("a")];
        for strategy in [ReconcileStrategy::Rebase, ReconcileStrategy::Merge] {
            assert_eq!(snapshot_scope(strategy, &remaining, None), None);
        }
    }

    #[test]
    fn a_moved_checkout_is_restored_with_the_working_copy() {
        let jj = OrderJj::default();
        point("pf", true).restore(&jj).expect("restore");
        assert_eq!(jj.calls(), vec!["restore pf"]);
    }

    #[test]
    fn a_checkout_left_alone_is_restored_without_touching_the_working_copy() {
        let jj = OrderJj::default();
        point("pf", false).restore(&jj).expect("restore");
        assert_eq!(jj.calls(), vec!["restore keeping pf"]);
    }
}
