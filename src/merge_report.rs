//! End-of-run summaries and follow-up advice shared by `merge` and `watch`.
//!
//! Each printer takes the writer it prints to (stdout in `merge` and `watch`)
//! so tests can read exactly what the user is told.

use std::io::Write;

use anyhow::Result;
use jjpr::merge;

/// The follow-up line for a run that stopped at a blocked segment.
///
/// `merge` and `watch` each have their own end-of-run summary, and they used to
/// choose this line independently. That is how `watch` shipped telling users to
/// re-run `watch` for a native stack, which never clears it, while `merge` said
/// the right thing. Deciding it in one place makes that divergence structurally
/// impossible; the printers only print.
///
/// `from_watch` selects the two genuine differences: watch names itself in the
/// fallback, and has no "run watch to wait" branch because it *is* the wait.
pub(crate) fn blocked_follow_up(reasons: &[merge::plan::BlockReason], from_watch: bool) -> String {
    if reasons
        .iter()
        .any(|r| matches!(r, merge::plan::BlockReason::NoPr))
    {
        return "\nRun `jjpr submit` to create PRs, then re-run `jjpr watch`.".to_string();
    }
    if let Some(advice) = native_stack_advice(reasons) {
        return advice;
    }
    if !from_watch && reasons.iter().all(|r| r.is_transient()) {
        return "\nRun `jjpr watch` to wait and auto-continue.".to_string();
    }
    if from_watch {
        // LocalSyncFailed / ForgeReconcileFailed don't normally reach here
        // because watch keeps iterating through them. Anything else here is
        // fatal and rerunning watch is the right action.
        "\nRun `jjpr watch` again once the issue is resolved.".to_string()
    } else {
        "\nRun `jjpr merge` again once the issue is resolved.".to_string()
    }
}

/// Follow-up advice for a stack jjpr cannot merge because it belongs to GitHub.
/// `None` when native-stack membership is not why it stopped.
///
/// Shared by the `merge` and `watch` summaries. Both otherwise end on a "try
/// again" line, and neither is true here: re-running resolves nothing, so the
/// only useful thing to say is which command does.
pub(crate) fn native_stack_advice(reasons: &[merge::plan::BlockReason]) -> Option<String> {
    reasons.iter().find_map(|r| match r {
        merge::plan::BlockReason::NativeStack { stack_number, .. } => Some(format!(
            "\nThis stack is registered as a GitHub native stack, which jjpr cannot merge.\n\
             Merge it with `gh stack merge`, or dissolve the native stack with\n\
             `gh stack unstack {stack_number}` to hand merging back to jjpr."
        )),
        _ => None,
    })
}

/// A failed write is ignored rather than panicking as `println!` would: the
/// report is advisory.
fn write(out: &mut impl Write, lines: &[String]) {
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
}

pub(crate) fn print_merge_summary(out: &mut impl Write, result: &merge::execute::MergeResult) {
    let line = if result.merged.is_empty()
        && result.skipped_merged.is_empty()
        && result.blocked_at.is_none()
    {
        "\nNo PRs to merge in this stack.".to_string()
    } else if let Some(ref blocked) = result.blocked_at {
        blocked_follow_up(&blocked.reasons, false)
    } else if result.merged.is_empty() && !result.skipped_merged.is_empty() {
        "\nAll PRs in this stack are already merged.".to_string()
    } else {
        format!(
            "\nDone. {} PR{} merged.",
            result.merged.len(),
            if result.merged.len() == 1 { "" } else { "s" }
        )
    };
    write(out, &[line]);
}

pub(crate) fn print_local_warnings(
    out: &mut impl Write,
    result: &merge::execute::MergeResult,
    segments: &[jjpr::jj::types::NarrowedSegment],
    stack_base: Option<&str>,
    default_branch: &str,
) -> Result<()> {
    use merge::execute::DivergenceKind;

    if result.local_warnings.is_empty() {
        return Ok(());
    }

    let merged_names: std::collections::HashSet<&str> = result
        .merged
        .iter()
        .map(|m| m.bookmark_name.as_str())
        .chain(
            result
                .skipped_merged
                .iter()
                .map(|s| s.bookmark_name.as_str()),
        )
        .collect();
    let unmerged: Vec<_> = segments
        .iter()
        .filter(|s| !merged_names.contains(s.bookmark.name.as_str()))
        .collect();

    let local_msgs: Vec<&str> = result
        .local_warnings
        .iter()
        .filter(|w| w.kind == DivergenceKind::Local)
        .map(|w| w.message.as_str())
        .collect();
    let forge_msgs: Vec<&str> = result
        .local_warnings
        .iter()
        .filter(|w| w.kind == DivergenceKind::Forge)
        .map(|w| w.message.as_str())
        .collect();

    merge::execute::print_fetch_rewrites(out, &result.local_warnings);

    let mut lines = Vec::new();
    if !local_msgs.is_empty() {
        lines.push(String::new());
        lines.push("Note: local state is out of sync with the forge:".to_string());
        lines.extend(local_msgs.iter().map(|m| format!("  {m}")));
        lines.push(String::new());
        lines.push("To accept the forge state (discard local divergence):".to_string());
        lines.push("  jj git fetch".to_string());
        for seg in &unmerged {
            lines.push(format!(
                "  jj bookmark set {} -r {}@origin",
                seg.bookmark.name, seg.bookmark.name
            ));
        }
        if let Some(first_unmerged) = unmerged.first() {
            lines.push(String::new());
            lines.push("Or to fix local state and push it to the forge:".to_string());
            let base = stack_base.unwrap_or(default_branch);
            // rebase_root: the OLDEST commit in the segment, so multi-commit
            // segments don't strand earlier commits under the old base.
            lines.push(format!(
                "  jj git fetch && jj rebase -s {} -d {base}",
                merge::execute::rebase_root(first_unmerged)
            ));
            lines.push("  # resolve any conflicts, then:".to_string());
            lines.push("  jjpr submit".to_string());
        }
    }

    if !forge_msgs.is_empty() {
        lines.push(String::new());
        lines.push("Note: forge reconcile failed:".to_string());
        lines.extend(forge_msgs.iter().map(|m| format!("  {m}")));
        lines.push(String::new());
        lines.push("Retry with `jjpr merge` (or wait for `jjpr watch` to retry).".to_string());
        lines.push(
            "Persistent failures may indicate a network or forge-permission issue.".to_string(),
        );
    }

    write(out, &lines);
    Ok(())
}

pub(crate) fn print_watch_summary(out: &mut impl Write, result: &jjpr::watch::WatchResult) {
    let mr = &result.merge_result;
    let mut lines = Vec::new();
    if !result.prs_created.is_empty() {
        let n = result.prs_created.len();
        lines.push(format!(
            "\n  Created {n} draft PR{}.",
            if n == 1 { "" } else { "s" }
        ));
    }
    if !result.prs_promoted.is_empty() {
        let n = result.prs_promoted.len();
        lines.push(format!(
            "  Promoted {n} PR{} to ready.",
            if n == 1 { "" } else { "s" }
        ));
    }
    lines.push(
        if mr.merged.is_empty() && mr.skipped_merged.is_empty() && mr.blocked_at.is_none() {
            "\nWatch ended without merging anything in this stack.".to_string()
        } else if let Some(ref blocked) = mr.blocked_at {
            blocked_follow_up(&blocked.reasons, true)
        } else if mr.merged.is_empty() && !mr.skipped_merged.is_empty() {
            "\nAll PRs in this stack are already merged.".to_string()
        } else {
            format!(
                "\nDone. {} PR{} merged.",
                mr.merged.len(),
                if mr.merged.len() == 1 { "" } else { "s" }
            )
        },
    );
    write(out, &lines);
}

#[cfg(test)]
mod tests {
    use super::*;
    use jjpr::jj::types::{Bookmark, NarrowedSegment};
    use jjpr::merge::execute::{
        BlockedPr, DivergenceKind, LocalDivergenceWarning, MergeResult, MergedPr, SkippedMergedPr,
    };
    use jjpr::merge::plan::BlockReason;
    use proptest::prelude::*;

    fn merged(name: &str) -> MergedPr {
        MergedPr {
            bookmark_name: name.to_string(),
            pr_number: 0,
            html_url: String::new(),
        }
    }

    fn skipped(name: &str) -> SkippedMergedPr {
        SkippedMergedPr {
            bookmark_name: name.to_string(),
            pr_number: 0,
        }
    }

    fn segment(name: &str) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.to_string(),
                commit_id: format!("commit_{name}"),
                change_id: format!("change_{name}"),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![],
            merge_source_names: vec![],
        }
    }

    fn warning(kind: DivergenceKind, message: &str) -> LocalDivergenceWarning {
        LocalDivergenceWarning {
            kind,
            message: message.to_string(),
        }
    }

    fn result(
        merged: Vec<MergedPr>,
        skipped_merged: Vec<SkippedMergedPr>,
        blocked_at: Option<BlockedPr>,
        local_warnings: Vec<LocalDivergenceWarning>,
    ) -> MergeResult {
        MergeResult {
            merged,
            blocked_at,
            skipped_merged,
            local_warnings,
        }
    }

    fn blocked(reasons: Vec<BlockReason>) -> BlockedPr {
        BlockedPr {
            bookmark_name: "b".to_string(),
            pr_number: Some(2),
            reasons,
        }
    }

    /// What a printer wrote, one entry per line.
    fn printed(print: impl FnOnce(&mut Vec<u8>)) -> Vec<String> {
        let mut out = Vec::new();
        print(&mut out);
        String::from_utf8(out)
            .expect("utf-8")
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn summary_counts_the_prs_merged() {
        let r = result(vec![merged("a"), merged("b")], vec![], None, vec![]);
        let lines = printed(|out| print_merge_summary(out, &r));
        assert_eq!(lines, ["", "Done. 2 PRs merged."]);
    }

    /// A PR merged this run is a merge, however many were already merged before it.
    #[test]
    fn summary_says_pr_not_prs_for_one_merged_beside_skipped() {
        let r = result(vec![merged("b")], vec![skipped("a")], None, vec![]);
        let lines = printed(|out| print_merge_summary(out, &r));
        assert_eq!(lines, ["", "Done. 1 PR merged."]);
    }

    #[test]
    fn summary_with_nothing_merged_and_nothing_blocked_says_so() {
        let r = result(vec![], vec![], None, vec![]);
        let lines = printed(|out| print_merge_summary(out, &r));
        assert_eq!(lines, ["", "No PRs to merge in this stack."]);
    }

    #[test]
    fn summary_with_only_skipped_prs_says_already_merged() {
        let r = result(vec![], vec![skipped("a")], None, vec![]);
        let lines = printed(|out| print_merge_summary(out, &r));
        assert_eq!(lines, ["", "All PRs in this stack are already merged."]);
    }

    #[test]
    fn summary_of_a_blocked_run_is_the_follow_up_for_merge() {
        let r = result(
            vec![merged("a")],
            vec![],
            Some(blocked(vec![BlockReason::ChecksPending])),
            vec![],
        );
        let lines = printed(|out| print_merge_summary(out, &r));
        assert_eq!(lines, ["", "Run `jjpr watch` to wait and auto-continue."]);
    }

    #[test]
    fn local_warnings_print_nothing_when_there_are_none() {
        let r = result(vec![merged("a")], vec![], None, vec![]);
        let lines = printed(|out| {
            print_local_warnings(out, &r, &[segment("a")], None, "main").expect("ok");
        });
        assert!(lines.is_empty(), "{lines:?}");
    }

    /// Only the segments still unmerged get recovery commands, and only local
    /// divergence gets the local section.
    #[test]
    fn local_divergence_lists_recovery_for_the_unmerged_segments_only() {
        let r = result(
            vec![merged("a")],
            vec![],
            None,
            vec![warning(DivergenceKind::Local, "bookmark b moved")],
        );
        let segments = [segment("a"), segment("b")];
        let lines = printed(|out| {
            print_local_warnings(out, &r, &segments, None, "main").expect("ok");
        });
        assert_eq!(
            lines,
            [
                "",
                "Note: local state is out of sync with the forge:",
                "  bookmark b moved",
                "",
                "To accept the forge state (discard local divergence):",
                "  jj git fetch",
                "  jj bookmark set b -r b@origin",
                "",
                "Or to fix local state and push it to the forge:",
                "  jj git fetch && jj rebase -s change_b -d main",
                "  # resolve any conflicts, then:",
                "  jjpr submit",
            ]
        );
    }

    #[test]
    fn the_rebase_hint_targets_the_stack_base_when_there_is_one() {
        let r = result(
            vec![],
            vec![],
            None,
            vec![warning(DivergenceKind::Local, "moved")],
        );
        let lines = printed(|out| {
            print_local_warnings(out, &r, &[segment("b")], Some("feature-base"), "main")
                .expect("ok");
        });
        assert!(
            lines.contains(&"  jj git fetch && jj rebase -s change_b -d feature-base".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn forge_divergence_says_to_retry_and_nothing_about_local_state() {
        let r = result(
            vec![],
            vec![],
            None,
            vec![warning(DivergenceKind::Forge, "update_pr_base: 502")],
        );
        let lines = printed(|out| {
            print_local_warnings(out, &r, &[segment("b")], None, "main").expect("ok");
        });
        assert_eq!(
            lines,
            [
                "",
                "Note: forge reconcile failed:",
                "  update_pr_base: 502",
                "",
                "Retry with `jjpr merge` (or wait for `jjpr watch` to retry).",
                "Persistent failures may indicate a network or forge-permission issue.",
            ]
        );
    }

    fn watch(
        created: usize,
        promoted: usize,
        merge_result: MergeResult,
    ) -> jjpr::watch::WatchResult {
        jjpr::watch::WatchResult {
            prs_created: (0..created)
                .map(|i| jjpr::watch::CreatedPr {
                    bookmark_name: format!("c{i}"),
                    pr_number: 1,
                })
                .collect(),
            prs_promoted: (0..promoted)
                .map(|i| jjpr::watch::PromotedPr {
                    bookmark_name: format!("p{i}"),
                    pr_number: 1,
                })
                .collect(),
            merge_result,
        }
    }

    #[test]
    fn watch_summary_counts_what_it_created_promoted_and_merged() {
        let w = watch(2, 1, result(vec![merged("a")], vec![], None, vec![]));
        let lines = printed(|out| print_watch_summary(out, &w));
        assert_eq!(
            lines,
            [
                "",
                "  Created 2 draft PRs.",
                "  Promoted 1 PR to ready.",
                "",
                "Done. 1 PR merged."
            ]
        );
    }

    #[test]
    fn watch_summary_is_singular_for_one_created_and_plural_for_promoted() {
        let w = watch(1, 2, result(vec![], vec![skipped("a")], None, vec![]));
        let lines = printed(|out| print_watch_summary(out, &w));
        assert_eq!(
            lines,
            [
                "",
                "  Created 1 draft PR.",
                "  Promoted 2 PRs to ready.",
                "",
                "All PRs in this stack are already merged.",
            ]
        );
    }

    /// A PR merged this run is a merge, however many were already merged before it.
    #[test]
    fn watch_summary_counts_the_merge_beside_already_merged_prs() {
        let w = watch(
            0,
            0,
            result(vec![merged("b")], vec![skipped("a")], None, vec![]),
        );
        let lines = printed(|out| print_watch_summary(out, &w));
        assert_eq!(lines, ["", "Done. 1 PR merged."]);
    }

    #[test]
    fn watch_summary_with_nothing_done_says_so() {
        let w = watch(0, 0, result(vec![], vec![], None, vec![]));
        let lines = printed(|out| print_watch_summary(out, &w));
        assert_eq!(
            lines,
            ["", "Watch ended without merging anything in this stack."]
        );
    }

    #[test]
    fn watch_summary_of_a_blocked_run_is_the_follow_up_for_watch() {
        let blocked_run = result(
            vec![merged("a")],
            vec![],
            Some(blocked(vec![BlockReason::ChecksPending])),
            vec![],
        );
        let lines = printed(|out| print_watch_summary(out, &watch(0, 0, blocked_run)));
        assert_eq!(
            lines,
            ["", "Run `jjpr watch` again once the issue is resolved."]
        );
    }

    fn a_reason() -> impl Strategy<Value = BlockReason> {
        prop_oneof![
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
        ]
    }

    proptest! {
        // The advice for a native stack is the same whichever command stopped on it:
        // neither `merge` nor `watch` can clear it, so neither may be recommended.
        #[test]
        fn native_stack_advice_is_the_same_from_merge_and_from_watch(
            stack_number in any::<u64>(),
            pr_number in any::<u64>(),
            position in any::<u32>(),
            size in any::<u32>(),
            others in proptest::collection::vec(a_reason(), 0..3),
        ) {
            let mut reasons = others;
            reasons.push(BlockReason::NativeStack { pr_number, stack_number, position, size });
            let from_merge = blocked_follow_up(&reasons, false);
            prop_assert_eq!(&from_merge, &blocked_follow_up(&reasons, true));
            let unstack = format!("`gh stack unstack {}`", stack_number);
            prop_assert!(from_merge.contains(&unstack), "{}", from_merge);
            prop_assert!(!from_merge.contains("jjpr merge") && !from_merge.contains("jjpr watch"));
        }

        // Every follow-up starts on its own line and names exactly one next command.
        #[test]
        fn a_follow_up_names_one_command_on_a_new_line(
            reasons in proptest::collection::vec(a_reason(), 1..4),
            from_watch in any::<bool>(),
        ) {
            let line = blocked_follow_up(&reasons, from_watch);
            prop_assert!(line.starts_with('\n'));
            prop_assert_eq!(line.matches("`jjpr ").count(), 1);
            if from_watch {
                prop_assert!(line.contains("`jjpr watch`"));
            }
        }
    }
}
