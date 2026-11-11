//! Pre-loop status line for `jjpr watch`.

use std::collections::HashSet;
use std::io::Write;

use crate::forge::Forge;
use crate::jj::Jj;
use crate::jj::types::NarrowedSegment;

/// One-time pre-loop summary so the user knows what watch is working
/// with. Best-effort: silently skip if the forge or jj queries fail.
pub(super) fn print_initial_watch_status(
    jj: &dyn Jj,
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
    target_bookmark: &str,
) {
    let Ok(initial_prs) = forge.list_open_prs(owner, repo) else {
        return;
    };
    let pr_map = crate::forge::build_pr_map(initial_prs, owner);
    let segments = super::rediscover_segments(jj, target_bookmark).unwrap_or_default();
    let with_pr: HashSet<&str> = pr_map.keys().map(String::as_str).collect();
    write_initial_watch_status(&mut std::io::stdout(), &segments, &with_pr);
}

/// The status line itself, from the segments found and the bookmarks that
/// already have a PR. A failed write is ignored rather than panicking as
/// `println!` would: the line is advisory.
fn write_initial_watch_status(
    out: &mut impl Write,
    segments: &[NarrowedSegment],
    bookmarks_with_pr: &HashSet<&str>,
) {
    let with_pr: Vec<_> = segments
        .iter()
        .filter(|s| bookmarks_with_pr.contains(s.bookmark.name.as_str()))
        .collect();
    let without_pr: Vec<_> = segments
        .iter()
        .filter(|s| !bookmarks_with_pr.contains(s.bookmark.name.as_str()))
        .collect();
    if with_pr.is_empty() && without_pr.is_empty() {
        return;
    }
    let plural = if segments.len() == 1 { "" } else { "s" };
    let with_pr_suffix = if !with_pr.is_empty() {
        format!(
            ", {} with existing PR{}",
            with_pr.len(),
            if with_pr.len() == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "  {} bookmark{plural} in stack{with_pr_suffix}",
        segments.len()
    );
    if !without_pr.is_empty() {
        let names: Vec<_> = without_pr
            .iter()
            .map(|s| s.bookmark.name.as_str())
            .collect();
        let _ = writeln!(out, "  Will create draft PRs for: {}\n", names.join(", "));
    } else {
        let _ = writeln!(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::types::Bookmark;

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

    fn status(segments: &[NarrowedSegment], with_pr: &[&str]) -> Vec<String> {
        let with_pr: HashSet<&str> = with_pr.iter().copied().collect();
        let mut out = Vec::new();
        write_initial_watch_status(&mut out, segments, &with_pr);
        String::from_utf8(out)
            .expect("utf-8")
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn an_empty_stack_prints_nothing() {
        assert!(status(&[], &[]).is_empty());
    }

    #[test]
    fn one_bookmark_with_a_pr_is_singular_throughout() {
        assert_eq!(
            status(&[segment("a")], &["a"]),
            ["  1 bookmark in stack, 1 with existing PR", ""]
        );
    }

    /// Only the bookmarks without a PR are promised a draft, and the count of
    /// existing PRs is the other side of the same split.
    #[test]
    fn bookmarks_without_a_pr_are_listed_for_draft_creation() {
        assert_eq!(
            status(&[segment("a"), segment("b"), segment("c")], &["a"]),
            [
                "  3 bookmarks in stack, 1 with existing PR",
                "  Will create draft PRs for: b, c",
                "",
            ]
        );
    }

    #[test]
    fn a_stack_with_no_prs_yet_has_no_existing_pr_count() {
        assert_eq!(
            status(&[segment("a"), segment("b")], &[]),
            [
                "  2 bookmarks in stack",
                "  Will create draft PRs for: a, b",
                "",
            ]
        );
    }

    #[test]
    fn several_existing_prs_are_plural() {
        assert_eq!(
            status(&[segment("a"), segment("b")], &["a", "b"]),
            ["  2 bookmarks in stack, 2 with existing PRs", ""]
        );
    }
}
