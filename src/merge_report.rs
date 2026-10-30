//! End-of-run summaries and follow-up advice shared by `merge` and `watch`.

use anyhow::Result;
use jjpr::merge;

/// Follow-up advice for a stack jjpr cannot merge because it belongs to GitHub.
/// `None` when native-stack membership is not why it stopped.
///
/// Shared by the `merge` and `watch` summaries. Both otherwise end on a "try
/// again" line, and neither is true here: re-running resolves nothing, so the
/// only useful thing to say is which command does.
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

pub(crate) fn print_merge_summary(result: &merge::execute::MergeResult) {
    if result.merged.is_empty() && result.skipped_merged.is_empty() && result.blocked_at.is_none() {
        println!("\nNo PRs to merge in this stack.");
    } else if let Some(ref blocked) = result.blocked_at {
        println!("{}", blocked_follow_up(&blocked.reasons, false));
    } else if result.merged.is_empty() && !result.skipped_merged.is_empty() {
        println!("\nAll PRs in this stack are already merged.");
    } else {
        println!(
            "\nDone. {} PR{} merged.",
            result.merged.len(),
            if result.merged.len() == 1 { "" } else { "s" }
        );
    }
}

pub(crate) fn print_local_warnings(
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

    merge::execute::print_fetch_rewrites(&mut std::io::stdout(), &result.local_warnings);

    if !local_msgs.is_empty() {
        println!();
        println!("Note: local state is out of sync with the forge:");
        for m in &local_msgs {
            println!("  {m}");
        }
        println!();
        println!("To accept the forge state (discard local divergence):");
        println!("  jj git fetch");
        for seg in &unmerged {
            println!(
                "  jj bookmark set {} -r {}@origin",
                seg.bookmark.name, seg.bookmark.name
            );
        }
        if let Some(first_unmerged) = unmerged.first() {
            println!();
            println!("Or to fix local state and push it to the forge:");
            let base = stack_base.unwrap_or(default_branch);
            // rebase_root: the OLDEST commit in the segment, so multi-commit
            // segments don't strand earlier commits under the old base.
            println!(
                "  jj git fetch && jj rebase -s {} -d {base}",
                merge::execute::rebase_root(first_unmerged)
            );
            println!("  # resolve any conflicts, then:");
            println!("  jjpr submit");
        }
    }

    if !forge_msgs.is_empty() {
        println!();
        println!("Note: forge reconcile failed:");
        for m in &forge_msgs {
            println!("  {m}");
        }
        println!();
        println!("Retry with `jjpr merge` (or wait for `jjpr watch` to retry).");
        println!("Persistent failures may indicate a network or forge-permission issue.");
    }

    Ok(())
}
