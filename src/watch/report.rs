//! Watch's inline reports. Each takes the writer it prints to (stdout in
//! watch) so tests can read exactly what the user is told.

use std::collections::HashMap;
use std::io::Write;

use crate::forge::ForgeKind;
use crate::forge::types::PullRequest;
use crate::jj::types::{Bookmark, NarrowedSegment};
use crate::merge::execute::{
    DivergenceKind, MergedPr, ReconcileState, SkippedMergedPr, format_block_reason, rebase_root,
};

/// A failed write is ignored rather than panicking as `println!` would: the
/// report is advisory.
fn write(out: &mut impl Write, lines: Vec<String>) {
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
}

/// Names open PRs for the user's bookmarks that watch neither merged nor
/// skipped. Prints nothing when there are none.
pub(super) fn orphaned_prs(
    out: &mut impl Write,
    my_bookmarks: &[Bookmark],
    pr_map: &HashMap<String, PullRequest>,
    merged: &[MergedPr],
    skipped: &[SkippedMergedPr],
    fk: ForgeKind,
) {
    let orphaned: Vec<_> = my_bookmarks
        .iter()
        .filter(|b| pr_map.contains_key(&b.name))
        .filter(|b| !merged.iter().any(|m| m.bookmark_name == b.name))
        .filter(|b| !skipped.iter().any(|s| s.bookmark_name == b.name))
        .collect();
    if orphaned.is_empty() {
        return;
    }
    let plural = if orphaned.len() == 1 { "" } else { "s" };
    let mut lines = vec![
        String::new(),
        format!(
            "  Note: {} open PR{plural} still exist for your bookmarks:",
            orphaned.len()
        ),
    ];
    for b in &orphaned {
        if let Some(pr) = pr_map.get(&b.name) {
            lines.push(format!("    - '{}' ({})", b.name, fk.format_ref(pr.number)));
        }
    }
    lines.push("  These may need manual attention.".to_string());
    write(out, lines);
}

/// The warnings and recovery hints when reconcile fails inside a watch
/// iteration. Mirrors `print_local_warnings` but tailored for the inline
/// "watch is going to keep trying" context. `base` is the stack base, or the
/// default branch when there is none.
pub(super) fn reconcile_failure(
    out: &mut impl Write,
    state: &ReconcileState,
    segments: &[NarrowedSegment],
    merged: &[MergedPr],
    skipped: &[SkippedMergedPr],
    base: &str,
    fk: ForgeKind,
) {
    let merged_names: std::collections::HashSet<&str> = merged
        .iter()
        .map(|m| m.bookmark_name.as_str())
        .chain(skipped.iter().map(|s| s.bookmark_name.as_str()))
        .collect();
    let next_unmerged = segments
        .iter()
        .find(|s| !merged_names.contains(s.bookmark.name.as_str()));

    let pr_label = next_unmerged
        .map(|s| format!(" '{}'", s.bookmark.name))
        .unwrap_or_default();

    let mut lines = vec![
        String::new(),
        format!("  Stopped before merging next PR{pr_label}:"),
    ];
    for reason in &state.block_reasons() {
        lines.push(format!("    - {}", format_block_reason(reason, fk)));
    }

    let warnings_of = |kind: DivergenceKind| {
        state
            .warnings
            .iter()
            .filter(move |w| w.kind == kind)
            .map(|w| format!("    {}", w.message))
    };

    if state.has_concurrent() {
        lines.push(String::new());
        lines.push("  Concurrent modification:".to_string());
        lines.extend(warnings_of(DivergenceKind::Concurrent));
        // No manual-fix hint: the warning already states that both sides' work
        // is preserved and watch retries next poll. Recovery never discards work,
        // so there's nothing for the user to restore.
    }

    if state.local_failed {
        lines.push(String::new());
        lines.push("  Local sync warnings:".to_string());
        lines.extend(warnings_of(DivergenceKind::Local));
        if let Some(seg) = next_unmerged {
            let name = &seg.bookmark.name;
            lines.extend([
                String::new(),
                "  To fix locally and continue (watch will resume on the next poll):".to_string(),
                // rebase_root: oldest commit in the segment so multi-commit
                // segments don't strand earlier commits.
                format!(
                    "    jj git fetch && jj rebase -s {} -d {base}",
                    rebase_root(seg)
                ),
                "  Or to accept the forge state:".to_string(),
                "    jj git fetch".to_string(),
                format!("    jj bookmark set {name} -r {name}@origin"),
            ]);
        }
    }

    if state.forge_failed {
        lines.push(String::new());
        lines.push("  Forge reconcile warnings:".to_string());
        lines.extend(warnings_of(DivergenceKind::Forge));
        lines.extend([
            String::new(),
            "  Watch will retry on the next poll. Persistent failures may indicate".to_string(),
            "  a network or forge-permission issue.".to_string(),
        ]);
    }
    write(out, lines);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::PullRequestRef;
    use crate::merge::execute::LocalDivergenceWarning;

    fn bookmark(name: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: format!("commit_{name}"),
            change_id: format!("change_{name}"),
            has_remote: true,
            is_synced: true,
        }
    }

    fn pr(name: &str, number: u64) -> PullRequest {
        let head = PullRequestRef {
            ref_name: name.to_string(),
            label: String::new(),
            sha: String::new(),
        };
        PullRequest {
            number,
            html_url: String::new(),
            title: name.to_string(),
            body: None,
            base: PullRequestRef {
                ref_name: "main".to_string(),
                ..head.clone()
            },
            head,
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
            author: String::new(),
            stack: None,
        }
    }

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
            bookmark: bookmark(name),
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

    /// What a report printed, one entry per line.
    fn printed(report: impl FnOnce(&mut Vec<u8>)) -> Vec<String> {
        let mut out = Vec::new();
        report(&mut out);
        String::from_utf8(out)
            .expect("utf-8")
            .lines()
            .map(String::from)
            .collect()
    }

    /// Merged and already-merged PRs are finished business; only the rest are
    /// orphans. Each filter has to drop exactly its own bookmark.
    #[test]
    fn orphaned_prs_names_only_prs_neither_merged_nor_skipped() {
        let bookmarks = ["a", "b", "c", "d"].map(bookmark);
        let prs = HashMap::from([
            ("a".to_string(), pr("a", 1)),
            ("b".to_string(), pr("b", 2)),
            ("c".to_string(), pr("c", 3)),
            ("d".to_string(), pr("d", 4)),
        ]);

        let lines = printed(|out| {
            orphaned_prs(
                out,
                &bookmarks,
                &prs,
                &[merged("a")],
                &[skipped("b")],
                ForgeKind::GitHub,
            )
        });

        assert_eq!(
            lines,
            [
                "",
                "  Note: 2 open PRs still exist for your bookmarks:",
                "    - 'c' (#3)",
                "    - 'd' (#4)",
                "  These may need manual attention.",
            ]
        );
    }

    #[test]
    fn orphaned_prs_says_pr_not_prs_for_one() {
        let prs = HashMap::from([("c".to_string(), pr("c", 3))]);

        let lines =
            printed(|out| orphaned_prs(out, &[bookmark("c")], &prs, &[], &[], ForgeKind::GitLab));

        assert_eq!(
            lines[1],
            "  Note: 1 open PR still exist for your bookmarks:"
        );
        assert_eq!(lines[2], "    - 'c' (!3)");
    }

    #[test]
    fn orphaned_prs_is_silent_when_nothing_is_left_open() {
        let prs = HashMap::from([("a".to_string(), pr("a", 1))]);

        let lines = printed(|out| {
            orphaned_prs(
                out,
                &[bookmark("a")],
                &prs,
                &[merged("a")],
                &[],
                ForgeKind::GitHub,
            )
        });

        assert!(lines.is_empty(), "got {lines:?}");
    }

    /// The report names the first segment not yet merged or skipped, and its
    /// recovery hints are for that segment, not for one already landed.
    #[test]
    fn reconcile_failure_names_the_next_unmerged_segment() {
        let state = ReconcileState {
            local_failed: true,
            warnings: vec![warning(DivergenceKind::Local, "local went wrong")],
            ..Default::default()
        };
        let segments = ["a", "b", "c"].map(segment);

        let lines = printed(|out| {
            reconcile_failure(
                out,
                &state,
                &segments,
                &[merged("a")],
                &[skipped("b")],
                "stack-base",
                ForgeKind::GitHub,
            )
        });

        assert_eq!(lines[1], "  Stopped before merging next PR 'c':");
        assert!(
            lines.contains(&"    jj git fetch && jj rebase -s change_c -d stack-base".to_string()),
            "the rebase hint must be for c onto the base, got {lines:?}"
        );
        assert!(
            lines.contains(&"    jj bookmark set c -r c@origin".to_string()),
            "the hint must be for 'c', got {lines:?}"
        );
    }

    /// Each section lists only its own kind of warning, so a user reading
    /// "Local sync warnings" is not sent to fix a forge problem locally.
    #[test]
    fn reconcile_failure_puts_each_warning_under_its_own_heading() {
        let state = ReconcileState {
            local_failed: true,
            forge_failed: true,
            warnings: vec![
                warning(DivergenceKind::Concurrent, "concurrent-msg"),
                warning(DivergenceKind::Local, "local-msg"),
                warning(DivergenceKind::Forge, "forge-msg"),
            ],
            ..Default::default()
        };

        let lines = printed(|out| {
            reconcile_failure(
                out,
                &state,
                &[segment("a")],
                &[],
                &[],
                "main",
                ForgeKind::GitHub,
            )
        });

        let heading = |h: &str| {
            lines
                .iter()
                .position(|l| l == h)
                .unwrap_or_else(|| panic!("no {h:?} in {lines:?}"))
        };
        let concurrent = heading("  Concurrent modification:");
        let local = heading("  Local sync warnings:");
        let forge = heading("  Forge reconcile warnings:");
        assert_eq!(lines[concurrent + 1], "    concurrent-msg");
        assert_eq!(lines[local + 1], "    local-msg");
        assert_eq!(lines[forge + 1], "    forge-msg");
        for msg in ["concurrent-msg", "local-msg", "forge-msg"] {
            assert_eq!(
                lines.iter().filter(|l| l.contains(msg)).count(),
                1,
                "{msg} must appear once, under its own heading: {lines:?}"
            );
        }
    }
}
