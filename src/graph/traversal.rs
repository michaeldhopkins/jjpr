use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::jj::types::{Bookmark, BookmarkSegment, LogEntry};
use crate::jj::Jj;

/// Result of traversing from a bookmark toward trunk.
pub struct TraversalResult {
    pub segments: Vec<BookmarkSegment>,
    pub seen_change_ids: HashSet<String>,
    pub has_merge: bool,
    /// If traversal stopped because it hit a fully_collected change, this is that change_id.
    /// Used to link the new segments to the existing graph.
    pub stopped_at: Option<String>,
}

/// Traverse from a bookmark's commit toward trunk, discovering segments.
///
/// A segment is a group of consecutive changes between two bookmarked changes
/// (or between trunk and a bookmarked change).
///
/// Stops early when hitting a change that was already fully collected.
/// Sets `has_merge` if any change has multiple parents.
pub fn traverse_and_discover_segments(
    jj: &dyn Jj,
    start_commit_id: &str,
    tainted: &HashSet<String>,
    fully_collected: &HashSet<String>,
    all_bookmarks: &HashMap<String, Bookmark>,
) -> Result<TraversalResult> {
    let mut segments: Vec<BookmarkSegment> = Vec::new();
    let mut current_segment_changes: Vec<LogEntry> = Vec::new();
    let mut current_segment_bookmarks: Vec<Bookmark> = Vec::new();
    let mut seen_change_ids: HashSet<String> = HashSet::new();
    let mut has_merge = false;

    let bookmark_change_ids: HashSet<&String> = all_bookmarks
        .values()
        .map(|b| &b.change_id)
        .collect();

    let entries = jj.get_branch_changes(start_commit_id)?;

    for entry in &entries {
        if entry.parents.len() > 1 {
            has_merge = true;
            seen_change_ids.insert(entry.change_id.clone());
            continue;
        }

        if has_merge {
            seen_change_ids.insert(entry.change_id.clone());
            continue;
        }

        if tainted.contains(&entry.change_id) {
            has_merge = true;
            seen_change_ids.insert(entry.change_id.clone());
            continue;
        }

        seen_change_ids.insert(entry.change_id.clone());

        // If this is already fully collected, stop
        if fully_collected.contains(&entry.change_id) {
            if !current_segment_changes.is_empty() {
                segments.push(BookmarkSegment {
                    bookmarks: std::mem::take(&mut current_segment_bookmarks),
                    changes: std::mem::take(&mut current_segment_changes),
                });
            }
            return Ok(TraversalResult {
                segments,
                seen_change_ids,
                has_merge,
                stopped_at: Some(entry.change_id.clone()),
            });
        }

        let is_bookmarked = bookmark_change_ids.contains(&entry.change_id);

        current_segment_changes.push(entry.clone());

        if is_bookmarked {
            let mut matching_bookmarks: Vec<Bookmark> = all_bookmarks
                .values()
                .filter(|b| b.change_id == entry.change_id)
                .cloned()
                .collect();
            matching_bookmarks.sort_by(|a, b| a.name.cmp(&b.name));
            current_segment_bookmarks.extend(matching_bookmarks);

            segments.push(BookmarkSegment {
                bookmarks: std::mem::take(&mut current_segment_bookmarks),
                changes: std::mem::take(&mut current_segment_changes),
            });
        }
    }

    // Flush remaining changes as a segment (unbookmarked tail)
    if !current_segment_changes.is_empty() {
        segments.push(BookmarkSegment {
            bookmarks: current_segment_bookmarks,
            changes: current_segment_changes,
        });
    }

    Ok(TraversalResult {
        segments,
        seen_change_ids,
        has_merge,
        stopped_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::types::GitRemote;
    use crate::jj::Jj;

    struct StubJj {
        entries: Vec<LogEntry>,
    }

    impl Jj for StubJj {
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(vec![])
        }
        fn get_branch_changes(&self, _to: &str) -> Result<Vec<LogEntry>> {
            Ok(self.entries.clone())
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
            Ok(vec![])
        }
        fn get_default_branch(&self) -> Result<String> {
            Ok("main".to_string())
        }
        fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> {
            Ok(())
        }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok("wc_commit".to_string())
        }
    }

    fn entry(
        commit_id: &str,
        change_id: &str,
        parents: Vec<&str>,
    ) -> LogEntry {
        LogEntry {
            commit_id: commit_id.to_string(),
            change_id: change_id.to_string(),
            author_name: "Test".to_string(),
            author_email: "test@test.com".to_string(),
            description: "test".to_string(),
            description_first_line: "test".to_string(),
            parents: parents.into_iter().map(|s| s.to_string()).collect(),
            local_bookmarks: vec![],
            remote_bookmarks: vec![],
            is_working_copy: false,
        }
    }

    #[test]
    fn test_empty_traversal() {
        let jj = StubJj { entries: vec![] };
        let result = traverse_and_discover_segments(
            &jj,
            "commit_a",
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert!(result.segments.is_empty());
        assert!(!result.has_merge);
    }

    #[test]
    fn test_merge_commit_detected() {
        let jj = StubJj {
            entries: vec![entry("c1", "ch1", vec!["p1", "p2"])],
        };
        let result = traverse_and_discover_segments(
            &jj,
            "c1",
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert!(result.has_merge);
    }

    #[test]
    fn test_single_bookmarked_change() {
        let bookmark = Bookmark {
            name: "feat".to_string(),
            commit_id: "c1".to_string(),
            change_id: "ch1".to_string(),
            has_remote: false,
            is_synced: false,
        };
        let all_bookmarks =
            HashMap::from([("feat".to_string(), bookmark)]);

        let jj = StubJj {
            entries: vec![entry("c1", "ch1", vec!["trunk"])],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "c1",
            &HashSet::new(),
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert_eq!(result.segments.len(), 1);
        assert_eq!(result.segments[0].bookmarks.len(), 1);
        assert_eq!(result.segments[0].bookmarks[0].name, "feat");
        assert_eq!(result.segments[0].changes.len(), 1);
    }

    #[test]
    fn test_stops_at_fully_collected() {
        let jj = StubJj {
            entries: vec![
                entry("c2", "ch2", vec!["c1"]),
                entry("c1", "ch1", vec!["trunk"]),
            ],
        };

        let fully_collected = HashSet::from(["ch1".to_string()]);

        let result = traverse_and_discover_segments(
            &jj,
            "c2",
            &HashSet::new(),
            &fully_collected,
            &HashMap::new(),
        )
        .unwrap();

        // Should have collected c2 but stopped at c1
        assert!(result.seen_change_ids.contains("ch2"));
        assert!(result.seen_change_ids.contains("ch1"));
    }
}
