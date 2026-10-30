//! The one-line explanation printed for each reason a segment is blocked.

use super::plan::BlockReason;
use crate::forge::ForgeKind;

pub(crate) fn format_block_reason(reason: &BlockReason, fk: ForgeKind) -> String {
    let abbr = fk.request_abbreviation();
    match reason {
        BlockReason::NoPr => format!("No {abbr} exists for this bookmark"),
        BlockReason::Draft => format!("{abbr} is still a draft"),
        BlockReason::ChecksFailing => "CI checks are failing".to_string(),
        BlockReason::ChecksPending => "CI checks are pending".to_string(),
        BlockReason::InsufficientApprovals { have, need } => {
            format!("Insufficient approvals ({have}/{need})")
        }
        BlockReason::ChangesRequested => "Changes have been requested".to_string(),
        BlockReason::Conflicted => "Has merge conflicts".to_string(),
        BlockReason::MergeabilityUnknown => {
            "Mergeability is still being computed (try again in a moment)".to_string()
        }
        BlockReason::LocalSyncFailed => "Local sync failed".to_string(),
        BlockReason::ForgeReconcileFailed => "Forge reconcile failed".to_string(),
        BlockReason::ConcurrentModification => {
            "Concurrent modification (another jj process)".to_string()
        }
        BlockReason::WorkingCopyRewritten => {
            "A fetch rewrote commits under your working copy".to_string()
        }
        BlockReason::NativeStack {
            pr_number,
            stack_number,
            position,
            size,
        } => {
            // At the bottom of the stack there is nothing below, so promising to
            // land "everything below it" would overstate what the command does.
            let lands = if *position <= 1 {
                format!("which lands #{pr_number}")
            } else {
                format!("which lands #{pr_number} and the {} below it", position - 1)
            };
            format!(
                "In native stack #{stack_number} ({position} of {size}); \
                 GitHub refuses API merges of stacked {abbr}s. \
                 Run `gh stack merge {pr_number}`, {lands}"
            )
        }
    }
}
