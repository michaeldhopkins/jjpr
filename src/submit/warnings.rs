//! Warnings and failure messages printed by submit (planner and executor).
//!
//! Each printer takes the writer it prints to (stdout for notes, stderr for
//! failures and warnings) so tests can read exactly what the user is told. A
//! failed write is ignored rather than panicking as `println!` would: the
//! message is advisory.

use std::io::Write;

use super::plan::SubmissionPlan;
use crate::forge::ForgeKind;

pub(super) fn print_title_drift_warnings(
    out: &mut impl Write,
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
        let _ = writeln!(
            out,
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
    out: &mut impl Write,
    conflicts: &[super::plan::BodyConflict],
    forge_kind: crate::forge::ForgeKind,
) {
    for conflict in conflicts {
        let reason = if conflict.unfingerprinted {
            "its description was edited on the forge (or predates jjpr tracking)"
        } else {
            "its description and commit message both changed since the last submit"
        };
        let _ = writeln!(
            out,
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

pub(super) fn report_partial_failure(out: &mut impl Write, completed: &[String]) {
    if !completed.is_empty() {
        let _ = writeln!(out, "\nThe following actions completed before the error:");
        for action in completed {
            let _ = writeln!(out, "  - {action}");
        }
        let _ = writeln!(out);
    }
}

/// Printed by the planner when a segment with an open PR has only empty
/// commits, so pushing it would close the PR.
pub(super) fn print_empty_segment_warning(out: &mut impl Write, bookmark_name: &str) {
    let _ = writeln!(
        out,
        "  Warning: '{bookmark_name}' has no file changes (all commits are empty)\n\
         \x20   Skipping push to avoid closing the existing PR.\n\
         \x20   hint: jj bookmark delete {bookmark_name} && jj git push --bookmark {bookmark_name}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::RepoInfo;
    use crate::jj::types::Bookmark;
    use crate::submit::plan::{BodyConflict, TitleDrift};

    fn bookmark(name: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: format!("commit_{name}"),
            change_id: format!("change_{name}"),
            has_remote: true,
            is_synced: true,
        }
    }

    fn drift(pr_number: u64, current: &str, expected: &str) -> TitleDrift {
        TitleDrift {
            bookmark: bookmark("b"),
            pr_number,
            current_title: current.to_string(),
            expected_title: expected.to_string(),
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
    fn title_drift_gives_the_gh_command_that_fixes_it() {
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };
        let lines = printed(|out| {
            print_title_drift_warnings(out, &[drift(7, "old", "new")], &repo, ForgeKind::GitHub);
        });
        assert_eq!(
            lines,
            [
                "  Note: #7 title differs from commit description",
                "         current: \"old\"",
                "         expected: \"new\"",
                "         fix with: gh pr edit 7 --repo o/r --title 'new'",
            ]
        );
    }

    /// The title goes inside single quotes on a shell line, so an apostrophe in
    /// it has to be escaped the shell's way.
    #[test]
    fn title_drift_on_gitlab_gives_glab_with_the_title_shell_quoted() {
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };
        let lines = printed(|out| {
            print_title_drift_warnings(out, &[drift(7, "old", "it's")], &repo, ForgeKind::GitLab);
        });
        assert_eq!(lines[0], "  Note: !7 title differs from commit description");
        assert_eq!(
            lines[3],
            "         fix with: glab mr update 7 --title 'it'\\''s'"
        );
    }

    #[test]
    fn body_conflicts_say_why_each_description_was_left_alone() {
        let conflicts = [
            BodyConflict {
                bookmark: bookmark("a"),
                pr_number: 3,
                unfingerprinted: true,
            },
            BodyConflict {
                bookmark: bookmark("b"),
                pr_number: 4,
                unfingerprinted: false,
            },
        ];
        let lines = printed(|out| print_body_conflict_warnings(out, &conflicts, ForgeKind::GitHub));
        assert_eq!(
            lines,
            [
                "  Note: left #3 body unchanged — its description was edited on the forge (or predates jjpr tracking).",
                "         Edit the commit message if you want jjpr to update the description.",
                "  Note: left #4 body unchanged — its description and commit message both changed since the last submit.",
                "         Edit the commit message if you want jjpr to update the description.",
            ]
        );
    }

    #[test]
    fn a_partial_failure_lists_what_completed() {
        let completed = ["pushed a".to_string(), "created PR #1".to_string()];
        let lines = printed(|out| report_partial_failure(out, &completed));
        assert_eq!(
            lines,
            [
                "",
                "The following actions completed before the error:",
                "  - pushed a",
                "  - created PR #1",
                "",
            ]
        );
    }

    #[test]
    fn a_failure_before_any_action_reports_nothing() {
        let lines = printed(|out| report_partial_failure(out, &[]));
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn the_empty_segment_warning_names_the_bookmark_and_the_way_out() {
        let lines = printed(|out| print_empty_segment_warning(out, "feat"));
        assert_eq!(
            lines,
            [
                "  Warning: 'feat' has no file changes (all commits are empty)",
                "    Skipping push to avoid closing the existing PR.",
                "    hint: jj bookmark delete feat && jj git push --bookmark feat",
            ]
        );
    }
}
