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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn a_reason() -> impl Strategy<Value = BlockReason> {
        prop_oneof![
            Just(BlockReason::NoPr),
            Just(BlockReason::Draft),
            Just(BlockReason::ChecksFailing),
            Just(BlockReason::ChecksPending),
            Just(BlockReason::ChangesRequested),
            Just(BlockReason::Conflicted),
            Just(BlockReason::MergeabilityUnknown),
            Just(BlockReason::LocalSyncFailed),
            Just(BlockReason::ForgeReconcileFailed),
            Just(BlockReason::ConcurrentModification),
            Just(BlockReason::WorkingCopyRewritten),
            (0u32..5, 1u32..5)
                .prop_map(|(have, need)| BlockReason::InsufficientApprovals { have, need }),
            (any::<u64>(), any::<u64>(), 1u32..20, 1u32..20).prop_map(
                |(pr_number, stack_number, position, size)| BlockReason::NativeStack {
                    pr_number,
                    stack_number,
                    position,
                    size
                }
            ),
        ]
    }

    fn a_forge() -> impl Strategy<Value = ForgeKind> {
        prop_oneof![
            Just(ForgeKind::GitHub),
            Just(ForgeKind::GitLab),
            Just(ForgeKind::Forgejo)
        ]
    }

    proptest! {
        // Each reason is one line under the blocked PR, whatever the forge.
        #[test]
        fn every_reason_is_explained_on_one_line(reason in a_reason(), fk in a_forge()) {
            let text = format_block_reason(&reason, fk);
            prop_assert!(!text.trim().is_empty());
            prop_assert!(!text.contains('\n'), "{text:?}");
        }

        // The native-stack explanation names the command that lands the PR, and promises
        // the PRs below it only when there are some.
        #[test]
        fn a_native_stack_reason_names_the_merge_command_and_what_it_lands(
            pr_number in any::<u64>(),
            stack_number in any::<u64>(),
            position in 1u32..20,
            size in 1u32..20,
            fk in a_forge(),
        ) {
            let reason = BlockReason::NativeStack { pr_number, stack_number, position, size };
            let text = format_block_reason(&reason, fk);
            let merge_command = format!("`gh stack merge {}`", pr_number);
            let place = format!("#{} ({} of {})", stack_number, position, size);
            prop_assert!(text.contains(&merge_command), "{}", text);
            prop_assert!(text.contains(&place), "{}", text);
            prop_assert_eq!(text.contains("below it"), position > 1);
        }
    }
}
