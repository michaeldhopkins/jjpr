//! What reconcile does when it finds a divergent change: gate, and name the cause.

use super::plan::BlockReason;
use crate::hints;
use crate::jj::Jj;
use crate::jj::types::WorkingCopyView;

/// Whether a reconcile warning came from local repo sync or from the
/// forge-side reconcile pass. Drives recovery hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DivergenceKind {
    /// `reconcile_local_state` failure: fetch, rebase, merge_into, push,
    /// divergent change ID, missing change ID, or local conflict.
    Local,
    /// `reconcile_forge_state` failure: list_open_prs, update_pr_base,
    /// or stack-comment update.
    Forge,
    /// A concurrent jj process reconciled the op log mid-reconcile; we paused
    /// before the mangling rebase (preserving both sides' work) and retry.
    /// Recovery is automatic and work-preserving, not a manual fix — so it
    /// renders differently from `Local`.
    Concurrent,
    /// A fetch rewrote the commits the working copy sits on and the stack came out
    /// divergent. Not another process: the user updates the working copy, and
    /// watch resumes on the next poll.
    FetchRewrite,
}

/// A warning recorded during reconcile_after_merge.
#[derive(Debug, Clone)]
pub struct LocalDivergenceWarning {
    pub kind: DivergenceKind,
    pub message: String,
}

/// Carrying state for reconcile_after_merge. Tracks whether each pass
/// failed and accumulates warning text. The gate consults `degraded()`
/// after each reconcile call to decide whether to stop merging.
#[derive(Debug, Default)]
pub struct ReconcileState {
    pub local_failed: bool,
    pub forge_failed: bool,
    pub warnings: Vec<LocalDivergenceWarning>,
    /// The next PR is in a GitHub native stack, so its base cannot be retargeted
    /// and jjpr cannot advance. Deliberately not a `*_failed` flag: nothing
    /// failed, and re-running changes nothing. It still stops the run.
    pub native_stack_block: Option<BlockReason>,
}

impl ReconcileState {
    pub fn degraded(&self) -> bool {
        self.local_failed
            || self.forge_failed
            || self.has_concurrent()
            || self.has_fetch_rewrite()
            || self.native_stack_block.is_some()
    }

    /// Whether this pass hit a concurrent op-log reconcile (and paused/rolled
    /// only the rebase back to preserve work). Derived from the warnings so the
    /// struct stays a plain flag/warning bag.
    pub fn has_concurrent(&self) -> bool {
        self.warnings
            .iter()
            .any(|w| w.kind == DivergenceKind::Concurrent)
    }

    /// Whether a fetch rewrote commits under the working copy this pass.
    pub fn has_fetch_rewrite(&self) -> bool {
        self.warnings
            .iter()
            .any(|w| w.kind == DivergenceKind::FetchRewrite)
    }

    /// Block reasons corresponding to the current failure flags. Returns
    /// an empty vec when the state is clean.
    pub fn block_reasons(&self) -> Vec<BlockReason> {
        let mut reasons = Vec::new();
        if self.has_concurrent() {
            reasons.push(BlockReason::ConcurrentModification);
        }
        if self.has_fetch_rewrite() {
            reasons.push(BlockReason::WorkingCopyRewritten);
        }
        if self.local_failed {
            reasons.push(BlockReason::LocalSyncFailed);
        }
        if self.forge_failed {
            reasons.push(BlockReason::ForgeReconcileFailed);
        }
        if let Some(reason) = &self.native_stack_block {
            reasons.push(reason.clone());
        }
        reasons
    }

    /// Wipe failure state so a follow-up reconcile gets a fresh chance.
    /// Used by run_watch_loop between iterations so the user can fix
    /// local state and have watch resume on the next poll.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Print the fetch-rewrote warnings, each as its own paragraph: they carry their own
/// instructions, so none of the rebase or bookmark-set commands that follow a failed sync.
pub fn print_fetch_rewrites(out: &mut impl std::io::Write, warnings: &[LocalDivergenceWarning]) {
    for w in warnings
        .iter()
        .filter(|w| w.kind == DivergenceKind::FetchRewrite)
    {
        let _ = write!(out, "\n{}\n", w.message);
    }
}

/// jj's corruption signal — divergent change ids.
pub(super) enum Divergence {
    Clean,
    Present(Vec<String>),
}

/// Read the divergence signal, failing SAFE. A read error here is almost always
/// lock contention from the very concurrent writer we're guarding against (jj's
/// op-heads lock; `run_jj` does not retry), and must NEVER be mistaken for
/// "clean" — that would let a mangled tree through the gate. So an error is
/// reported as `Present` (with no ids to name), gating the caller.
pub(super) fn divergence(jj: &dyn Jj) -> Divergence {
    match jj.divergent_change_ids() {
        Ok(d) if d.is_empty() => Divergence::Clean,
        Ok(d) => Divergence::Present(d),
        Err(_) => Divergence::Present(Vec::new()),
    }
}

/// A work-preserving concurrent-modification warning. jj's reconcile keeps both
/// sides' commits, so recovery never discards work: we either gate before the
/// mangling rebase (`restored = false`) or roll only the rebase back to the
/// clean post-fetch op (`restored = true`). The message still names `jj op restore` for
/// anything that looks missing.
pub(super) fn concurrent_gate_warning(
    restored: bool,
    divergent_ids: &[String],
) -> LocalDivergenceWarning {
    LocalDivergenceWarning {
        kind: DivergenceKind::Concurrent,
        message: hints::concurrent_pause(restored, divergent_ids),
    }
}

/// Whether the fetch is what made `ids` divergent: the commits `@` sits on are no longer
/// the ones read before it, and one of the divergent changes is `@` or one of those
/// parents. Another jj process restacking under `@` also moves its parents, so the
/// divergent change has to be one `@` was built on for the fetch to take the blame. A
/// read that fails, or none taken, cannot say so and answers no.
fn fetch_rewrote_working_copy(
    jj: &dyn Jj,
    before: Option<&WorkingCopyView>,
    ids: &[String],
) -> bool {
    let (Some(before), Ok(after)) = (before, jj.working_copy_view()) else {
        return false;
    };
    let sorted = |v: &[String]| {
        let mut v = v.to_vec();
        v.sort();
        v
    };
    !before.parents.is_empty()
        && sorted(&before.parents) != sorted(&after.parents)
        && ids.iter().any(|id| before.change_ids.contains(id))
}

/// The warning for a rebase that came out divergent, after the restore to the post-fetch
/// operation (`restored`) was tried. The fetch takes the blame under the same test as
/// [`gate`]; otherwise another process raced the rebase.
pub(super) fn rebase_gate(
    jj: &dyn Jj,
    before: Option<&WorkingCopyView>,
    ids: &[String],
    restored: bool,
) -> LocalDivergenceWarning {
    if fetch_rewrote_working_copy(jj, before, ids) {
        return fetch_rewrite_warning();
    }
    concurrent_gate_warning(restored, if restored { &[] } else { ids })
}

fn fetch_rewrite_warning() -> LocalDivergenceWarning {
    LocalDivergenceWarning {
        kind: DivergenceKind::FetchRewrite,
        message: hints::fetch_rewrote_working_copy(),
    }
}

/// The gate when the stack is divergent, `None` when it is clean. `before` is `@` and its
/// parents read before the fetch (`None` before there was one).
pub(super) fn gate(
    jj: &dyn Jj,
    before: Option<&WorkingCopyView>,
) -> Option<LocalDivergenceWarning> {
    let Divergence::Present(ids) = divergence(jj) else {
        return None;
    };
    if fetch_rewrote_working_copy(jj, before, &ids) {
        return Some(fetch_rewrite_warning());
    }
    Some(concurrent_gate_warning(false, &ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(kind: DivergenceKind, message: &str) -> LocalDivergenceWarning {
        LocalDivergenceWarning {
            kind,
            message: message.to_string(),
        }
    }

    #[test]
    fn only_fetch_rewrites_are_printed_each_in_its_own_paragraph() {
        let mut out = Vec::new();
        print_fetch_rewrites(
            &mut out,
            &[
                warning(DivergenceKind::Local, "local"),
                warning(DivergenceKind::FetchRewrite, "rewrote"),
                warning(DivergenceKind::Concurrent, "concurrent"),
            ],
        );
        assert_eq!(String::from_utf8(out).unwrap(), "\nrewrote\n");
    }

    #[test]
    fn the_state_names_a_fetch_rewrite_as_its_own_block_reason() {
        let mut state = ReconcileState::default();
        assert!(!state.degraded());
        state
            .warnings
            .push(warning(DivergenceKind::FetchRewrite, "w"));
        assert!(state.has_fetch_rewrite());
        assert!(!state.has_concurrent());
        assert!(state.degraded());
        assert_eq!(
            state.block_reasons(),
            vec![BlockReason::WorkingCopyRewritten]
        );
    }
}
