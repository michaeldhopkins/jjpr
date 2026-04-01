use anyhow::Result;

use crate::jj::types::{Bookmark, BookmarkSegment, NarrowedSegment};

/// When a segment has multiple bookmarks, the user must pick one for PR creation.
/// If running non-interactively, takes the first bookmark.
pub fn resolve_bookmark_selections(
    segments: &[BookmarkSegment],
    interactive: bool,
) -> Result<Vec<NarrowedSegment>> {
    segments
        .iter()
        .map(|segment| {
            let bookmark = if segment.bookmarks.len() == 1 {
                segment.bookmarks[0].clone()
            } else if segment.bookmarks.is_empty() {
                anyhow::bail!("segment has no bookmarks — this is an internal error");
            } else if interactive {
                select_bookmark_interactive(&segment.bookmarks)?
            } else {
                let chosen = &segment.bookmarks[0];
                eprintln!(
                    "  Using bookmark '{}' (multiple bookmarks on this change)",
                    chosen.name
                );
                chosen.clone()
            };

            Ok(NarrowedSegment {
                bookmark,
                changes: segment.changes.clone(),
                merge_source_names: segment.merge_source_names.clone(),
            })
        })
        .collect()
}

fn select_bookmark_interactive(bookmarks: &[Bookmark]) -> Result<Bookmark> {
    let names: Vec<&str> = bookmarks.iter().map(|b| b.name.as_str()).collect();

    let selection = dialoguer::Select::new()
        .with_prompt("Multiple bookmarks on this change — select one for the PR")
        .items(&names)
        .default(0)
        .interact()
        .map_err(|e| anyhow::anyhow!("selection cancelled: {e}"))?;

    Ok(bookmarks[selection].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::types::LogEntry;

    fn bookmark(name: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: format!("c_{name}"),
            change_id: format!("ch_{name}"),
            has_remote: false,
            is_synced: false,
        }
    }

    fn segment(bookmarks: Vec<Bookmark>) -> BookmarkSegment {
        BookmarkSegment {
            bookmarks,
            changes: vec![LogEntry {
                commit_id: "c1".to_string(),
                change_id: "ch1".to_string(),
                author_name: "Test".to_string(),
                author_email: "test@test.com".to_string(),
                description: "test".to_string(),
                description_first_line: "test".to_string(),
                parents: vec![],
                local_bookmarks: vec![],
                remote_bookmarks: vec![],
                is_working_copy: false,
                conflict: false,
                empty: false,
            }],
            merge_source_names: vec![],
        }
    }

    #[test]
    fn test_single_bookmark_per_segment() {
        let segments = vec![segment(vec![bookmark("auth")])];
        let result = resolve_bookmark_selections(&segments, false).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].bookmark.name, "auth");
    }

    #[test]
    fn test_multiple_bookmarks_non_interactive_takes_first() {
        let segments = vec![segment(vec![bookmark("a"), bookmark("b")])];
        let result = resolve_bookmark_selections(&segments, false).unwrap();
        assert_eq!(result[0].bookmark.name, "a");
    }

    #[test]
    fn test_merge_source_names_propagated() {
        let mut seg = segment(vec![bookmark("merge-feat")]);
        seg.merge_source_names = vec!["feat-d".to_string()];
        let segments = vec![seg];
        let result = resolve_bookmark_selections(&segments, false).unwrap();
        assert_eq!(
            result[0].merge_source_names,
            vec!["feat-d"],
            "merge_source_names should propagate from BookmarkSegment to NarrowedSegment"
        );
    }

    #[test]
    fn test_empty_bookmarks_errors() {
        let segments = vec![BookmarkSegment {
            bookmarks: vec![],
            changes: vec![],
            merge_source_names: vec![],
        }];
        assert!(resolve_bookmark_selections(&segments, false).is_err());
    }
}
