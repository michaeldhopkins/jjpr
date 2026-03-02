use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::jj::types::{Bookmark, BookmarkSegment, LogEntry};
use crate::jj::Jj;

/// Result of traversing from a bookmark toward trunk.
pub struct TraversalResult {
    pub segments: Vec<BookmarkSegment>,
    pub seen_change_ids: HashSet<String>,
    pub has_merge: bool,
    /// When has_merge is true, the change_id of the first merge commit found.
    pub merge_change_id: Option<String>,
    /// When has_merge is true, the parent commit_ids of the merge commit.
    pub merge_parent_ids: Vec<String>,
    /// If traversal stopped because it hit a fully_collected change, this is that change_id.
    /// Used to link the new segments to the existing graph.
    pub stopped_at: Option<String>,
    /// If traversal stopped at a commit with a remote bookmark not owned by the user,
    /// this is the branch name (e.g., "coworker-auth"). Used to set the stack's base branch.
    pub foreign_base: Option<String>,
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
    fully_collected: &HashSet<String>,
    all_bookmarks: &HashMap<String, Bookmark>,
) -> Result<TraversalResult> {
    let mut segments: Vec<BookmarkSegment> = Vec::new();
    let mut current_segment_changes: Vec<LogEntry> = Vec::new();
    let mut current_segment_bookmarks: Vec<Bookmark> = Vec::new();
    let mut seen_change_ids: HashSet<String> = HashSet::new();
    let mut has_merge = false;
    let mut merge_change_id: Option<String> = None;
    let mut merge_parent_ids: Vec<String> = Vec::new();

    let bookmark_change_ids: HashSet<&String> = all_bookmarks
        .values()
        .map(|b| &b.change_id)
        .collect();

    let entries = jj.get_changes_to_commit(start_commit_id)?;

    for entry in &entries {
        if entry.parents.len() > 1 {
            has_merge = true;
            if merge_change_id.is_none() {
                merge_change_id = Some(entry.change_id.clone());
                merge_parent_ids = entry.parents.clone();
            }
            seen_change_ids.insert(entry.change_id.clone());
            continue;
        }

        if has_merge {
            seen_change_ids.insert(entry.change_id.clone());
            continue;
        }

        // Check for foreign remote bookmarks before adding this entry.
        // A foreign bookmark is one pushed by someone else — its branch name
        // is not in all_bookmarks (the user's own bookmarks).
        let foreign = entry
            .remote_bookmarks
            .iter()
            .filter(|rb| !rb.ends_with("@git"))
            .filter_map(|rb| rb.rsplit_once('@').map(|(name, _remote)| name))
            .find(|name| !all_bookmarks.contains_key(*name));

        if let Some(foreign_name) = foreign {
            if !current_segment_changes.is_empty() {
                segments.push(BookmarkSegment {
                    bookmarks: std::mem::take(&mut current_segment_bookmarks),
                    changes: std::mem::take(&mut current_segment_changes),
                    merge_source_names: vec![],
                });
            }
            return Ok(TraversalResult {
                segments,
                seen_change_ids,
                has_merge,
                merge_change_id: merge_change_id.clone(),
                merge_parent_ids: merge_parent_ids.clone(),
                stopped_at: None,
                foreign_base: Some(foreign_name.to_string()),
            });
        }

        seen_change_ids.insert(entry.change_id.clone());

        // If this is already fully collected, stop
        if fully_collected.contains(&entry.change_id) {
            if !current_segment_changes.is_empty() {
                segments.push(BookmarkSegment {
                    bookmarks: std::mem::take(&mut current_segment_bookmarks),
                    changes: std::mem::take(&mut current_segment_changes),
                    merge_source_names: vec![],
                });
            }
            return Ok(TraversalResult {
                segments,
                seen_change_ids,
                has_merge,
                merge_change_id: merge_change_id.clone(),
                merge_parent_ids: merge_parent_ids.clone(),
                stopped_at: Some(entry.change_id.clone()),
                foreign_base: None,
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
                merge_source_names: vec![],
            });
        }
    }

    // Flush remaining changes as a segment (unbookmarked tail)
    if !current_segment_changes.is_empty() {
        segments.push(BookmarkSegment {
            bookmarks: current_segment_bookmarks,
            changes: current_segment_changes,
            merge_source_names: vec![],
        });
    }

    Ok(TraversalResult {
        segments,
        seen_change_ids,
        has_merge,
        merge_change_id,
        merge_parent_ids,
        stopped_at: None,
        foreign_base: None,
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
        fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> {
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
        fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> { unimplemented!() }
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
            &HashMap::new(),
        )
        .unwrap();
        assert!(result.has_merge);
        assert_eq!(result.merge_change_id.as_deref(), Some("ch1"));
        assert_eq!(result.merge_parent_ids, vec!["p1", "p2"]);
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
            &fully_collected,
            &HashMap::new(),
        )
        .unwrap();

        // Should have collected c2 but stopped at c1
        assert!(result.seen_change_ids.contains("ch2"));
        assert!(result.seen_change_ids.contains("ch1"));
    }

    fn entry_with_remote_bookmarks(
        commit_id: &str,
        change_id: &str,
        parents: Vec<&str>,
        remote_bookmarks: Vec<&str>,
    ) -> LogEntry {
        let mut e = entry(commit_id, change_id, parents);
        e.remote_bookmarks = remote_bookmarks.into_iter().map(|s| s.to_string()).collect();
        e
    }

    #[test]
    fn test_foreign_remote_bookmark_stops_traversal() {
        // Two entries: user's change on top, coworker's commit at bottom
        let jj = StubJj {
            entries: vec![
                entry("c2", "ch2", vec!["c1"]),
                entry_with_remote_bookmarks(
                    "c1", "ch1", vec!["trunk"],
                    vec!["coworker-feat@origin"],
                ),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "c2",
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(result.foreign_base, Some("coworker-feat".to_string()));
        assert_eq!(result.segments.len(), 1);
        assert_eq!(result.segments[0].changes.len(), 1);
        assert_eq!(result.segments[0].changes[0].change_id, "ch2");
    }

    #[test]
    fn test_own_remote_bookmark_continues() {
        let bookmark = Bookmark {
            name: "my-feat".to_string(),
            commit_id: "c1".to_string(),
            change_id: "ch1".to_string(),
            has_remote: true,
            is_synced: true,
        };
        let all_bookmarks = HashMap::from([("my-feat".to_string(), bookmark)]);

        let jj = StubJj {
            entries: vec![
                entry("c2", "ch2", vec!["c1"]),
                entry_with_remote_bookmarks(
                    "c1", "ch1", vec!["trunk"],
                    vec!["my-feat@origin"],
                ),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "c2",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert!(result.foreign_base.is_none());
        // Both entries should be collected
        assert!(result.seen_change_ids.contains("ch1"));
        assert!(result.seen_change_ids.contains("ch2"));
    }

    #[test]
    fn test_git_remote_ignored() {
        // @git is jj's internal tracking, not a real foreign remote
        let jj = StubJj {
            entries: vec![
                entry_with_remote_bookmarks(
                    "c1", "ch1", vec!["trunk"],
                    vec!["something@git"],
                ),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "c1",
            &HashSet::new(),
            &HashMap::new(),
        )
        .unwrap();

        assert!(result.foreign_base.is_none());
        assert!(result.seen_change_ids.contains("ch1"));
    }

    #[test]
    fn test_foreign_base_flushes_pending_segment() {
        // user change (unbookmarked) -> coworker's commit with remote bookmark
        // The user's change should be flushed as a segment
        let bookmark = Bookmark {
            name: "my-feat".to_string(),
            commit_id: "c3".to_string(),
            change_id: "ch3".to_string(),
            has_remote: false,
            is_synced: false,
        };
        let all_bookmarks = HashMap::from([("my-feat".to_string(), bookmark)]);

        let jj = StubJj {
            entries: vec![
                entry("c3", "ch3", vec!["c2"]),
                entry("c2", "ch2", vec!["c1"]),
                entry_with_remote_bookmarks(
                    "c1", "ch1", vec!["trunk"],
                    vec!["coworker-base@origin"],
                ),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "c3",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert_eq!(result.foreign_base, Some("coworker-base".to_string()));
        // c3 is bookmarked, so it forms its own segment; c2 is unbookmarked,
        // flushed as a tail segment when we hit the foreign base
        assert_eq!(result.segments.len(), 2);
        // First segment: my-feat (c3)
        assert_eq!(result.segments[0].bookmarks[0].name, "my-feat");
        // Second segment: unbookmarked tail (c2)
        assert_eq!(result.segments[1].changes[0].change_id, "ch2");
    }
}
