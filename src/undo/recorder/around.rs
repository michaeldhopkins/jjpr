//! [`Recorder::around`]: telling jjpr's own jj operations from everyone
//! else's.

use super::Recorder;

/// Operations that bring work into a span which jjpr did not do itself.
const FOREIGN: [&str; 2] = ["snapshot working copy", "reconcile divergent operations"];

/// How many operations [`Recorder::around`] reads back, more than one jj
/// command makes or a user makes between two of jjpr's.
const OWN_GAP: usize = 100;

const UNREADABLE: &str = "more operations than jjpr could read";

impl Recorder {
    /// Run one of jjpr's own jj commands that can change the repo, noting
    /// the operations it made. Operations made by anyone else since jjpr's
    /// previous command, and working-copy snapshots taken during this one,
    /// are noted as absorbed: undoing the entry would discard them.
    pub fn around<T>(&self, run: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.track(true, run)
    }

    /// [`Recorder::around`] for a fetch, which is not work to undo: the entry
    /// goes on from where it left the repo, but a poll that only fetched is
    /// not recorded.
    pub fn around_fetch(&self, run: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.track(false, run)
    }

    fn track<T>(&self, work: bool, run: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let before = self.repo.current_op().ok();
        let result = run();
        let after = self.repo.current_op().ok();
        let last_own = self.lock().last_own.clone();
        let mut absorbed = Vec::new();
        if let Some(before) = &before
            && *before != last_own
        {
            match self.repo.ops_since(&last_own, OWN_GAP) {
                Ok(Some(ops)) => absorbed.extend(
                    ops.into_iter()
                        .skip_while(|o| o.id != *before)
                        .map(|o| o.description),
                ),
                _ => absorbed.push(UNREADABLE.to_string()),
            }
        }
        if let (Some(before), Some(after)) = (&before, &after)
            && before != after
        {
            match self.repo.ops_since(before, OWN_GAP) {
                Ok(Some(ops)) => absorbed.extend(
                    ops.into_iter()
                        .map(|o| o.description)
                        .filter(|d| FOREIGN.iter().any(|f| d.starts_with(f))),
                ),
                _ => absorbed.push(UNREADABLE.to_string()),
            }
        }
        let mut inner = self.lock();
        if let Some(after) = after {
            inner.last_own = after.clone();
            inner.worked |= work && before.as_ref() != Some(&after);
            if let Some(mut entry) = inner.entry.take() {
                for a in absorbed {
                    if !entry.absorbed.contains(&a) {
                        entry.absorbed.push(a);
                    }
                }
                // Kept current, so an entry whose process died still says
                // where jjpr's own work ended.
                entry.end_op = Some(after);
                if inner.written {
                    self.save(&mut inner, &entry);
                }
                inner.entry = Some(entry);
            }
        }
        result
    }
}
