use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::jj::types::{Bookmark, BookmarkSegment, LogEntry};
use crate::jj::Jj;

/// Result of traversing from a bookmark toward trunk.
pub struct TraversalResult {
    pub segments: Vec<BookmarkSegment>,
    pub seen_change_ids: HashSet<String>,
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
/// When a merge commit is encountered, follows `parents[0]` and skips other arms.
/// Skipped entries are NOT added to `seen_change_ids` so they remain available
/// for independent bookmark traversal.
pub fn traverse_and_discover_segments(
    jj: &dyn Jj,
    start_commit_id: &str,
    fully_collected: &HashSet<String>,
    all_bookmarks: &HashMap<String, Bookmark>,
) -> Result<TraversalResult> {
    let mut segments: Vec<BookmarkSegment> = Vec::new();
    let mut current_segment_changes: Vec<LogEntry> = Vec::new();
    let mut current_segment_bookmarks: Vec<Bookmark> = Vec::new();
    let mut current_segment_merge_source_names: Vec<String> = Vec::new();
    let mut seen_change_ids: HashSet<String> = HashSet::new();

    // After a merge, tracks which commit_ids are on our followed path.
    // None means no merge encountered yet (all entries are on path).
    let mut on_path: Option<HashSet<String>> = None;

    let bookmark_change_ids: HashSet<&String> = all_bookmarks
        .values()
        .map(|b| &b.change_id)
        .collect();

    // Reverse map: commit_id → bookmark name (for resolving merge parent names)
    let commit_id_to_bookmark: HashMap<&String, &String> = all_bookmarks
        .values()
        .map(|b| (&b.commit_id, &b.name))
        .collect();

    let entries = jj.get_changes_to_commit(start_commit_id)?;

    for entry in &entries {
        // If we're past a merge, skip entries not on the followed path
        if let Some(ref path) = on_path
            && !path.contains(&entry.commit_id)
        {
            continue;
        }

        // Handle merge commits: pick first parent, skip others
        if entry.parents.len() > 1 {
            let followed_parent = entry.parents[0].clone();
            let skipped_names: Vec<String> = entry.parents[1..]
                .iter()
                .map(|cid| {
                    commit_id_to_bookmark
                        .get(cid)
                        .map(|n| (*n).clone())
                        .unwrap_or_else(|| cid[..cid.len().min(12)].to_string())
                })
                .collect();

            current_segment_merge_source_names.extend(skipped_names);

            let path = on_path.get_or_insert_with(HashSet::new);
            path.insert(followed_parent);
            // Don't insert skipped parents — their entries will be skipped

            // Fall through to process this merge entry normally
        } else if let Some(ref mut path) = on_path {
            // Linear entry after a merge — add its parents to the followed path
            for parent in &entry.parents {
                path.insert(parent.clone());
            }
        }

        // Check for foreign remote bookmarks before adding this entry.
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
                    merge_source_names: std::mem::take(&mut current_segment_merge_source_names),
                });
            }
            return Ok(TraversalResult {
                segments,
                seen_change_ids,
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
                    merge_source_names: std::mem::take(&mut current_segment_merge_source_names),
                });
            }
            return Ok(TraversalResult {
                segments,
                seen_change_ids,
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
                merge_source_names: std::mem::take(&mut current_segment_merge_source_names),
            });
        }
    }

    // Flush remaining changes as a segment (unbookmarked tail)
    if !current_segment_changes.is_empty() {
        segments.push(BookmarkSegment {
            bookmarks: current_segment_bookmarks,
            changes: current_segment_changes,
            merge_source_names: current_segment_merge_source_names,
        });
    }

    Ok(TraversalResult {
        segments,
        seen_change_ids,
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
        fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> { unimplemented!() }
        fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
            Ok(vec!["dummy_commit_id".to_string()])
        }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> { Ok(false) }
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
            conflict: false,
            empty: false,
        }
    }

    fn make_bookmark(name: &str, commit_id: &str, change_id: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: commit_id.to_string(),
            change_id: change_id.to_string(),
            has_remote: false,
            is_synced: false,
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
    }

    #[test]
    fn test_merge_commit_followed_through() {
        // B (merge of C and D) -> C -> trunk
        // B should be included as a segment, C should be processed
        let b_bookmark = make_bookmark("feat-b", "cb", "chb");
        let c_bookmark = make_bookmark("feat-c", "cc", "chc");
        let d_bookmark = make_bookmark("feat-d", "cd", "chd");
        let all_bookmarks = HashMap::from([
            ("feat-b".to_string(), b_bookmark),
            ("feat-c".to_string(), c_bookmark),
            ("feat-d".to_string(), d_bookmark),
        ]);

        let jj = StubJj {
            entries: vec![
                entry("cb", "chb", vec!["cc", "cd"]),  // merge
                entry("cc", "chc", vec!["trunk"]),      // followed parent
                entry("cd", "chd", vec!["trunk"]),      // skipped parent
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cb",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert_eq!(result.segments.len(), 2);
        // First segment (leaf): B with merge info
        assert_eq!(result.segments[0].bookmarks[0].name, "feat-b");
        assert_eq!(result.segments[0].merge_source_names, vec!["feat-d"]);
        // Second segment: C (followed parent)
        assert_eq!(result.segments[1].bookmarks[0].name, "feat-c");
        assert!(result.segments[1].merge_source_names.is_empty());
    }

    #[test]
    fn test_merge_skipped_entries_not_in_seen() {
        // Skipped arm entries should not appear in seen_change_ids
        let b_bookmark = make_bookmark("feat-b", "cb", "chb");
        let all_bookmarks = HashMap::from([
            ("feat-b".to_string(), b_bookmark),
        ]);

        let jj = StubJj {
            entries: vec![
                entry("cb", "chb", vec!["cc", "cd"]),
                entry("cc", "chc", vec!["trunk"]),
                entry("cd", "chd", vec!["trunk"]),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cb",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert!(result.seen_change_ids.contains("chb"));
        assert!(result.seen_change_ids.contains("chc"));
        assert!(!result.seen_change_ids.contains("chd"), "skipped arm should not be in seen");
    }

    #[test]
    fn test_merge_source_names_resolved() {
        // Skipped parents with bookmarks should resolve to bookmark names
        let b_bookmark = make_bookmark("feat-b", "cb", "chb");
        let d_bookmark = make_bookmark("feat-d", "cd", "chd");
        let all_bookmarks = HashMap::from([
            ("feat-b".to_string(), b_bookmark),
            ("feat-d".to_string(), d_bookmark),
        ]);

        let jj = StubJj {
            entries: vec![
                entry("cb", "chb", vec!["cc", "cd"]),
                entry("cc", "chc", vec!["trunk"]),
                entry("cd", "chd", vec!["trunk"]),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cb",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert_eq!(result.segments[0].merge_source_names, vec!["feat-d"]);
    }

    #[test]
    fn test_merge_source_names_fallback_to_commit_id() {
        // Skipped parent without bookmark falls back to short commit_id
        let b_bookmark = make_bookmark("feat-b", "cb", "chb");
        let all_bookmarks = HashMap::from([
            ("feat-b".to_string(), b_bookmark),
        ]);

        let jj = StubJj {
            entries: vec![
                entry("cb", "chb", vec!["cc", "cd_long_commit_id"]),
                entry("cc", "chc", vec!["trunk"]),
                entry("cd_long_commit_id", "chd", vec!["trunk"]),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cb",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert_eq!(result.segments[0].merge_source_names, vec!["cd_long_comm"]);
    }

    #[test]
    fn test_nested_merge() {
        // A -> B (merge C,D) -> C (merge E,F) -> E -> trunk
        // Both merges followed through first parent
        let b_bookmark = make_bookmark("feat-b", "cb", "chb");
        let c_bookmark = make_bookmark("feat-c", "cc", "chc");
        let e_bookmark = make_bookmark("feat-e", "ce", "che");
        let d_bookmark = make_bookmark("feat-d", "cd", "chd");
        let f_bookmark = make_bookmark("feat-f", "cf", "chf");
        let all_bookmarks = HashMap::from([
            ("feat-b".to_string(), b_bookmark),
            ("feat-c".to_string(), c_bookmark),
            ("feat-d".to_string(), d_bookmark),
            ("feat-e".to_string(), e_bookmark),
            ("feat-f".to_string(), f_bookmark),
        ]);

        let jj = StubJj {
            entries: vec![
                entry("cb", "chb", vec!["cc", "cd"]),  // B merges C,D
                entry("cc", "chc", vec!["ce", "cf"]),   // C merges E,F
                entry("ce", "che", vec!["trunk"]),       // followed
                entry("cf", "chf", vec!["trunk"]),       // skipped (by C)
                entry("cd", "chd", vec!["trunk"]),       // skipped (by B)
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cb",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert_eq!(result.segments.len(), 3);
        assert_eq!(result.segments[0].bookmarks[0].name, "feat-b");
        assert_eq!(result.segments[0].merge_source_names, vec!["feat-d"]);
        assert_eq!(result.segments[1].bookmarks[0].name, "feat-c");
        assert_eq!(result.segments[1].merge_source_names, vec!["feat-f"]);
        assert_eq!(result.segments[2].bookmarks[0].name, "feat-e");
        assert!(result.segments[2].merge_source_names.is_empty());

        // D and F should NOT be in seen
        assert!(!result.seen_change_ids.contains("chd"));
        assert!(!result.seen_change_ids.contains("chf"));
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
        assert!(result.segments[0].merge_source_names.is_empty());
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
        assert!(result.seen_change_ids.contains("ch1"));
        assert!(result.seen_change_ids.contains("ch2"));
    }

    #[test]
    fn test_git_remote_ignored() {
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
        assert_eq!(result.segments.len(), 2);
        assert_eq!(result.segments[0].bookmarks[0].name, "my-feat");
        assert_eq!(result.segments[1].changes[0].change_id, "ch2");
    }

    #[test]
    fn test_merge_with_three_parents() {
        let b_bookmark = make_bookmark("feat-b", "cb", "chb");
        let c_bookmark = make_bookmark("feat-c", "cc", "chc");
        let d_bookmark = make_bookmark("feat-d", "cd", "chd");
        let e_bookmark = make_bookmark("feat-e", "ce", "che");
        let all_bookmarks = HashMap::from([
            ("feat-b".to_string(), b_bookmark),
            ("feat-c".to_string(), c_bookmark),
            ("feat-d".to_string(), d_bookmark),
            ("feat-e".to_string(), e_bookmark),
        ]);

        let jj = StubJj {
            entries: vec![
                entry("cb", "chb", vec!["cc", "cd", "ce"]),  // 3-parent merge
                entry("cc", "chc", vec!["trunk"]),
                entry("cd", "chd", vec!["trunk"]),
                entry("ce", "che", vec!["trunk"]),
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cb",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        assert_eq!(result.segments[0].merge_source_names, vec!["feat-d", "feat-e"]);
        assert!(!result.seen_change_ids.contains("chd"));
        assert!(!result.seen_change_ids.contains("che"));
    }

    #[test]
    fn test_unbookmarked_merge_before_bookmarked() {
        // Leaf(bookmarked) → Merge(unbookmarked, parents: p1, p2) → trunk
        // Merge info lands on the unbookmarked tail, not the bookmarked segment.
        let leaf = make_bookmark("leaf", "cl", "chl");
        let all_bookmarks = HashMap::from([("leaf".to_string(), leaf)]);

        let jj = StubJj {
            entries: vec![
                entry("cl", "chl", vec!["cm"]),            // bookmarked leaf
                entry("cm", "chm", vec!["cp1", "cp2"]),    // unbookmarked merge
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cl",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        // Leaf's segment should have no merge info (it's not a merge)
        assert_eq!(result.segments[0].bookmarks[0].name, "leaf");
        assert!(
            result.segments[0].merge_source_names.is_empty(),
            "bookmark above merge should not carry merge note"
        );
        // The merge info is on the unbookmarked tail segment
        assert_eq!(result.segments.len(), 2);
        assert!(!result.segments[1].merge_source_names.is_empty());
    }

    #[test]
    fn test_consecutive_unbookmarked_merges_accumulate() {
        // Leaf(bookmarked) → M1(unbookmarked, merge of X,Y) → M2(unbookmarked, merge of Z,W) → Root(bookmarked)
        // Both merges' source names should accumulate, not overwrite.
        let leaf = make_bookmark("leaf", "cl", "chl");
        let root = make_bookmark("root", "cr", "chr");
        let all_bookmarks = HashMap::from([
            ("leaf".to_string(), leaf),
            ("root".to_string(), root),
        ]);

        let jj = StubJj {
            entries: vec![
                entry("cl", "chl", vec!["cm1"]),             // bookmarked leaf
                entry("cm1", "chm1", vec!["cm2", "cy"]),     // unbookmarked merge 1
                entry("cm2", "chm2", vec!["cr", "cw"]),      // unbookmarked merge 2 (on followed path)
                entry("cr", "chr", vec!["trunk"]),            // bookmarked root
                entry("cy", "chy", vec!["trunk"]),            // skipped by M1
                entry("cw", "chw", vec!["trunk"]),            // skipped by M2
            ],
        };

        let result = traverse_and_discover_segments(
            &jj,
            "cl",
            &HashSet::new(),
            &all_bookmarks,
        )
        .unwrap();

        // Leaf gets its own segment (no merge info)
        assert_eq!(result.segments[0].bookmarks[0].name, "leaf");
        assert!(result.segments[0].merge_source_names.is_empty());

        // Root's segment contains M1 and M2 (unbookmarked) plus Root.
        // Both merges' source names should be accumulated.
        assert_eq!(result.segments[1].bookmarks[0].name, "root");
        assert_eq!(result.segments[1].merge_source_names.len(), 2);
        // cy (short commit_id fallback) from M1, cw from M2
        assert!(result.segments[1].merge_source_names.contains(&"cy".to_string()));
        assert!(result.segments[1].merge_source_names.contains(&"cw".to_string()));
    }
}
