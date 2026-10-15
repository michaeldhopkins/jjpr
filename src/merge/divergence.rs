//! What reconcile does when it finds a divergent change: gate, and name the cause.

use super::execute::{DivergenceKind, LocalDivergenceWarning};
use crate::hints;
use crate::jj::Jj;

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
/// clean post-fetch op (`restored = true`). No "run jj op restore" hand-off.
pub(super) fn concurrent_gate_warning(
    restored: bool,
    divergent_ids: &[String],
) -> LocalDivergenceWarning {
    LocalDivergenceWarning {
        kind: DivergenceKind::Concurrent,
        message: hints::concurrent_pause(restored, divergent_ids),
    }
}

/// Whether the commits `@` sits on are not the ones they were before the fetch.
/// Only a fetch rewriting them (the remote amended or force-pushed a commit the
/// working copy is built on) does that between two reads a moment apart; a read
/// that fails, or none taken, cannot say so and answers no.
fn fetch_rewrote_parents(jj: &dyn Jj, before: Option<&[String]>) -> bool {
    let (Some(before), Ok(after)) = (before, jj.working_copy_parents()) else {
        return false;
    };
    let sorted = |v: &[String]| {
        let mut v = v.to_vec();
        v.sort();
        v
    };
    !before.is_empty() && sorted(before) != sorted(&after)
}

/// The gate when the stack is divergent, `None` when it is clean. `parents_before` is
/// `@`'s parents read before the fetch (`None` before there was one): a divergence that
/// comes with them changed is the fetch's, not another jj process's.
pub(super) fn gate(
    jj: &dyn Jj,
    parents_before: Option<&[String]>,
) -> Option<LocalDivergenceWarning> {
    let Divergence::Present(ids) = divergence(jj) else {
        return None;
    };
    if !ids.is_empty() && fetch_rewrote_parents(jj, parents_before) {
        return Some(LocalDivergenceWarning {
            kind: DivergenceKind::Local,
            message: hints::fetch_rewrote_working_copy(),
        });
    }
    Some(concurrent_gate_warning(false, &ids))
}
