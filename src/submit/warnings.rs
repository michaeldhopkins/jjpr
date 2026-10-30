//! Warnings and failure messages printed by submit (planner and executor).

use super::plan::SubmissionPlan;
use crate::forge::ForgeKind;

pub(super) fn print_title_drift_warnings(
    drifts: &[super::plan::TitleDrift],
    repo_info: &crate::forge::types::RepoInfo,
    forge_kind: crate::forge::ForgeKind,
) {
    for drift in drifts {
        let escaped_title = drift.expected_title.replace('\'', "'\\''");
        let fix_hint = match forge_kind {
            crate::forge::ForgeKind::GitHub | crate::forge::ForgeKind::Forgejo => format!(
                "gh pr edit {} --repo {}/{} --title '{}'",
                drift.pr_number, repo_info.owner, repo_info.repo, escaped_title,
            ),
            crate::forge::ForgeKind::GitLab => format!(
                "glab mr update {} --title '{}'",
                drift.pr_number, escaped_title,
            ),
        };
        println!(
            "  Note: {} title differs from commit description\n\
             \x20        current: \"{}\"\n\
             \x20        expected: \"{}\"\n\
             \x20        fix with: {fix_hint}",
            forge_kind.format_ref(drift.pr_number),
            drift.current_title,
            drift.expected_title,
        );
    }
}

pub(super) fn print_body_conflict_warnings(
    conflicts: &[super::plan::BodyConflict],
    forge_kind: crate::forge::ForgeKind,
) {
    for conflict in conflicts {
        let reason = if conflict.unfingerprinted {
            "its description was edited on the forge (or predates jjpr tracking)"
        } else {
            "its description and commit message both changed since the last submit"
        };
        println!(
            "  Note: left {} body unchanged — {reason}.\n\
             \x20        Edit the commit message if you want jjpr to update the description.",
            forge_kind.format_ref(conflict.pr_number),
        );
    }
}

/// Explain a refusal caused by native-stack membership.
///
/// Deliberately says what jjpr wanted to do, why it cannot, and what the user
/// can do instead. The bare `422` ("Validation Failed") that GitHub would
/// otherwise surface names none of those.
pub(super) fn native_stack_conflict_message(plan: &SubmissionPlan, fk: ForgeKind) -> String {
    let abbr = fk.request_abbreviation();
    let mut out =
        String::from("the stack's shape changed, but its PRs are in a GitHub native stack.\n\n");
    for c in &plan.native_stack_base_conflicts {
        out.push_str(&format!(
            "  {} ('{}') needs its base moved from '{}' to '{}'\n",
            fk.format_ref(c.pr_number),
            c.bookmark.name,
            c.current_base,
            c.expected_base,
        ));
    }
    // One jjpr stack can span more than one native stack: a native stack may be
    // rooted on any branch, so a user can register (a,b) as one and (c,d) as
    // another within the same chain. Naming only the first would leave the user
    // blocked again on the next run, so unstack every stack involved.
    let mut stacks: Vec<u64> = plan
        .native_stack_base_conflicts
        .iter()
        .map(|c| c.stack_number)
        .collect();
    stacks.sort_unstable();
    stacks.dedup();
    let unstack = stacks
        .iter()
        .map(|n| format!("gh stack unstack {n}"))
        .collect::<Vec<_>>()
        .join(" && ");

    out.push_str(&format!(
        "\nGitHub does not allow retargeting a {abbr} that belongs to a stack, so \
         jjpr cannot\napply that. Nothing has been pushed.\n\n\
         To continue, dissolve the native stack and let jjpr manage the bases:\n\
         \x20 {unstack}\n\
         \x20 jjpr submit\n\n\
         Or keep the native stack and restructure it with gh instead:\n\
         \x20 {unstack} && gh stack init <branches in the new order>",
    ));
    out
}

pub(super) fn report_partial_failure(completed: &[String]) {
    if !completed.is_empty() {
        eprintln!("\nThe following actions completed before the error:");
        for action in completed {
            eprintln!("  - {action}");
        }
        eprintln!();
    }
}

/// Printed by the planner when a segment with an open PR has only empty
/// commits, so pushing it would close the PR.
pub(super) fn print_empty_segment_warning(bookmark_name: &str) {
    eprintln!("  Warning: '{bookmark_name}' has no file changes (all commits are empty)");
    eprintln!("    Skipping push to avoid closing the existing PR.");
    eprintln!(
        "    hint: jj bookmark delete {bookmark_name} && jj git push --bookmark {bookmark_name}"
    );
}
