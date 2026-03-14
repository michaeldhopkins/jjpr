use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::jj::types::{Bookmark, BookmarkSegment, BranchStack, LogEntry};
use crate::jj::Jj;

use super::traversal;

/// The full graph of bookmarked changes and their relationships.
#[derive(Debug, Clone)]
pub struct ChangeGraph {
    pub bookmarks: HashMap<String, Bookmark>,
    pub bookmark_to_change_id: HashMap<String, String>,
    /// child change_id -> parent change_id (single parent only, linear stacks)
    pub adjacency_list: HashMap<String, String>,
    /// bookmarked change_id -> the changes in that segment
    pub change_id_to_segment: HashMap<String, Vec<LogEntry>>,
    pub stack_leafs: HashSet<String>,
    pub stack_roots: HashSet<String>,
    pub stacks: Vec<BranchStack>,
}

/// Build the change graph from the current jj repo state.
pub fn build_change_graph(jj: &dyn Jj) -> Result<ChangeGraph> {
    let bookmarks = jj.get_my_bookmarks()?;

    let mut all_bookmarks: HashMap<String, Bookmark> = HashMap::new();
    let mut bookmark_to_change_id: HashMap<String, String> = HashMap::new();
    let mut adjacency_list: HashMap<String, String> = HashMap::new();
    let mut change_id_to_segment: HashMap<String, Vec<LogEntry>> = HashMap::new();
    let mut fully_collected: HashSet<String> = HashSet::new();
    // Maps root change_id → foreign branch name for stacks based on non-trunk branches
    let mut foreign_bases: HashMap<String, String> = HashMap::new();
    // Maps segment change_id → merge source names for segments containing merge commits
    let mut merge_source_map: HashMap<String, Vec<String>> = HashMap::new();

    for bookmark in &bookmarks {
        all_bookmarks.insert(bookmark.name.clone(), bookmark.clone());
        bookmark_to_change_id.insert(bookmark.name.clone(), bookmark.change_id.clone());
    }

    // Traverse each bookmark toward trunk, discovering segments
    for bookmark in &bookmarks {
        let result = traversal::traverse_and_discover_segments(
            jj,
            &bookmark.commit_id,
            &fully_collected,
            &all_bookmarks,
        )?;

        // Record segments and adjacencies.
        // Segments are ordered leaf-to-root; adjacency maps child → parent.
        let mut prev_change_id: Option<String> = None;
        for segment in &result.segments {
            if let Some(first_change) = segment.changes.first() {
                let segment_change_id = segment
                    .bookmarks
                    .first()
                    .map(|b| b.change_id.clone())
                    .unwrap_or_else(|| first_change.change_id.clone());

                change_id_to_segment
                    .insert(segment_change_id.clone(), segment.changes.clone());

                if !segment.merge_source_names.is_empty() {
                    merge_source_map.insert(
                        segment_change_id.clone(),
                        segment.merge_source_names.clone(),
                    );
                }

                if let Some(prev) = &prev_change_id {
                    adjacency_list.insert(prev.clone(), segment_change_id.clone());
                }
                prev_change_id = Some(segment_change_id.clone());

                fully_collected.insert(segment_change_id);
            }
        }

        // Link the last discovered segment to the already-collected change
        if let (Some(last), Some(stopped)) = (&prev_change_id, &result.stopped_at) {
            adjacency_list.insert(last.clone(), stopped.clone());
        }

        // Track foreign base for this path's root
        if let (Some(root), Some(base)) = (&prev_change_id, &result.foreign_base) {
            foreign_bases.insert(root.clone(), base.clone());
        }

        for change_id in result.seen_change_ids {
            fully_collected.insert(change_id);
        }
    }

    // Identify leafs and roots
    let parents: HashSet<&String> = adjacency_list.values().collect();
    let children: HashSet<&String> = adjacency_list.keys().collect();

    let stack_leafs: HashSet<String> = children
        .iter()
        .filter(|id| !parents.contains(*id))
        .map(|id| id.to_string())
        .chain(
            // Bookmarks not in any adjacency relationship are standalone leafs
            bookmarks
                .iter()
                .filter(|b| {
                    !adjacency_list.contains_key(&b.change_id)
                        && !parents.contains(&b.change_id)
                })
                .map(|b| b.change_id.clone()),
        )
        .collect();

    let stack_roots: HashSet<String> = parents
        .iter()
        .filter(|id| !children.contains(*id))
        .map(|id| id.to_string())
        .collect();

    // Group into stacks by walking from each leaf to its root
    let stacks = build_stacks(
        &stack_leafs,
        &adjacency_list,
        &change_id_to_segment,
        &all_bookmarks,
        &foreign_bases,
        &merge_source_map,
    );

    Ok(ChangeGraph {
        bookmarks: all_bookmarks,
        bookmark_to_change_id,
        adjacency_list,
        change_id_to_segment,
        stack_leafs,
        stack_roots,
        stacks,
    })
}

fn build_stacks(
    leafs: &HashSet<String>,
    adjacency_list: &HashMap<String, String>,
    change_id_to_segment: &HashMap<String, Vec<LogEntry>>,
    bookmarks: &HashMap<String, Bookmark>,
    foreign_bases: &HashMap<String, String>,
    merge_source_map: &HashMap<String, Vec<String>>,
) -> Vec<BranchStack> {
    // Invert adjacency: parent -> child, so we can walk from root to leaf
    let mut parent_to_child: HashMap<&String, &String> = HashMap::new();
    for (child, parent) in adjacency_list {
        parent_to_child.insert(parent, child);
    }

    let mut stacks = Vec::new();

    let mut sorted_leafs: Vec<&String> = leafs.iter().collect();
    sorted_leafs.sort();

    for leaf in sorted_leafs {
        // Walk from leaf toward root to collect the full path
        let mut path = vec![leaf.clone()];
        let mut current = leaf;
        while let Some(parent) = adjacency_list.get(current) {
            path.push(parent.clone());
            current = parent;
        }
        path.reverse(); // now root -> leaf

        let segments: Vec<BookmarkSegment> = path
            .iter()
            .filter_map(|change_id| {
                let changes = change_id_to_segment.get(change_id)?.clone();
                let mut segment_bookmarks: Vec<Bookmark> = bookmarks
                    .values()
                    .filter(|b| b.change_id == *change_id)
                    .cloned()
                    .collect();
                segment_bookmarks.sort_by(|a, b| a.name.cmp(&b.name));
                // Skip unbookmarked segments (e.g. merge tails toward trunk)
                if segment_bookmarks.is_empty() {
                    return None;
                }
                Some(BookmarkSegment {
                    bookmarks: segment_bookmarks,
                    changes,
                    merge_source_names: merge_source_map
                        .get(change_id)
                        .cloned()
                        .unwrap_or_default(),
                })
            })
            .collect();

        if !segments.is_empty() {
            let base_branch = path.first().and_then(|root| foreign_bases.get(root)).cloned();
            stacks.push(BranchStack { segments, base_branch });
        }
    }

    stacks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::types::GitRemote;
    use crate::jj::Jj;

    /// Stub Jj that returns canned data.
    struct StubJj {
        bookmarks: Vec<Bookmark>,
        log_entries: HashMap<String, Vec<LogEntry>>,
    }

    impl Jj for StubJj {
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(self.bookmarks.clone())
        }
        fn get_changes_to_commit(&self, to_commit_id: &str) -> Result<Vec<LogEntry>> {
            Ok(self
                .log_entries
                .get(to_commit_id)
                .cloned()
                .unwrap_or_default())
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
    }

    fn make_log_entry(
        commit_id: &str,
        change_id: &str,
        parents: Vec<&str>,
        bookmarks: Vec<&str>,
    ) -> LogEntry {
        LogEntry {
            commit_id: commit_id.to_string(),
            change_id: change_id.to_string(),
            author_name: "Test".to_string(),
            author_email: "test@test.com".to_string(),
            description: "test".to_string(),
            description_first_line: "test".to_string(),
            parents: parents.into_iter().map(|s| s.to_string()).collect(),
            local_bookmarks: bookmarks.into_iter().map(|s| s.to_string()).collect(),
            remote_bookmarks: vec![],
            is_working_copy: false,
            conflict: false,
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
    fn test_empty_repo() {
        let jj = StubJj {
            bookmarks: vec![],
            log_entries: HashMap::new(),
        };
        let graph = build_change_graph(&jj).unwrap();
        assert!(graph.stacks.is_empty());
        assert!(graph.bookmarks.is_empty());
    }

    #[test]
    fn test_single_bookmark_linear_stack() {
        // trunk -> commit_a (bookmarked "feature")
        let jj = StubJj {
            bookmarks: vec![make_bookmark("feature", "commit_a", "change_a")],
            log_entries: HashMap::from([(
                "commit_a".to_string(),
                vec![make_log_entry(
                    "commit_a",
                    "change_a",
                    vec!["trunk"],
                    vec!["feature"],
                )],
            )]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.bookmarks.len(), 1);
        assert!(graph.bookmarks.contains_key("feature"));
    }

    #[test]
    fn test_multi_bookmark_stack() {
        // trunk -> commit_a (auth) -> commit_b (profile)
        // Querying "commit_b" returns both entries in reverse order.
        let jj = StubJj {
            bookmarks: vec![
                make_bookmark("auth", "commit_a", "change_a"),
                make_bookmark("profile", "commit_b", "change_b"),
            ],
            log_entries: HashMap::from([
                (
                    "commit_a".to_string(),
                    vec![make_log_entry(
                        "commit_a",
                        "change_a",
                        vec!["trunk"],
                        vec!["auth"],
                    )],
                ),
                (
                    "commit_b".to_string(),
                    vec![
                        make_log_entry("commit_b", "change_b", vec!["commit_a"], vec!["profile"]),
                        make_log_entry("commit_a", "change_a", vec!["trunk"], vec!["auth"]),
                    ],
                ),
            ]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.bookmarks.len(), 2);
        assert!(!graph.stacks.is_empty());

        // Verify the stack has both segments in order
        let stack = &graph.stacks[0];
        assert_eq!(stack.segments.len(), 2);
        assert_eq!(stack.segments[0].bookmarks[0].name, "auth");
        assert_eq!(stack.segments[1].bookmarks[0].name, "profile");
    }

    #[test]
    fn test_merge_commit_included_in_stack() {
        // A merge bookmark should now be included in a stack, not excluded.
        let jj = StubJj {
            bookmarks: vec![make_bookmark("feature", "commit_a", "change_a")],
            log_entries: HashMap::from([(
                "commit_a".to_string(),
                vec![make_log_entry(
                    "commit_a",
                    "change_a",
                    vec!["p1", "p2"],
                    vec!["feature"],
                )],
            )]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.stacks.len(), 1);
        assert_eq!(graph.stacks[0].segments[0].bookmarks[0].name, "feature");
        assert_eq!(graph.stacks[0].segments[0].merge_source_names, vec!["p2"]);
    }

    #[test]
    fn test_two_independent_stacks() {
        // Two bookmarks with separate ancestries form independent stacks.
        let jj = StubJj {
            bookmarks: vec![
                make_bookmark("alpha", "commit_a", "change_a"),
                make_bookmark("beta", "commit_b", "change_b"),
            ],
            log_entries: HashMap::from([
                (
                    "commit_a".to_string(),
                    vec![make_log_entry(
                        "commit_a",
                        "change_a",
                        vec!["trunk"],
                        vec!["alpha"],
                    )],
                ),
                (
                    "commit_b".to_string(),
                    vec![make_log_entry(
                        "commit_b",
                        "change_b",
                        vec!["trunk"],
                        vec!["beta"],
                    )],
                ),
            ]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.bookmarks.len(), 2);
        // Each bookmark is its own stack (no adjacency relationship)
        assert_eq!(graph.stacks.len(), 2);
    }

    fn make_log_entry_with_remote_bookmarks(
        commit_id: &str,
        change_id: &str,
        parents: Vec<&str>,
        bookmarks: Vec<&str>,
        remote_bookmarks: Vec<&str>,
    ) -> LogEntry {
        let mut e = make_log_entry(commit_id, change_id, parents, bookmarks);
        e.remote_bookmarks = remote_bookmarks.into_iter().map(|s| s.to_string()).collect();
        e
    }

    #[test]
    fn test_stack_with_foreign_base() {
        // trunk -> coworker_commit (foreign remote bookmark) -> commit_a (bookmarked "feature")
        let jj = StubJj {
            bookmarks: vec![make_bookmark("feature", "commit_a", "change_a")],
            log_entries: HashMap::from([(
                "commit_a".to_string(),
                vec![
                    make_log_entry("commit_a", "change_a", vec!["coworker_c"], vec!["feature"]),
                    make_log_entry_with_remote_bookmarks(
                        "coworker_c", "coworker_ch", vec!["trunk"],
                        vec![], vec!["coworker-feat@origin"],
                    ),
                ],
            )]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.stacks.len(), 1);
        assert_eq!(
            graph.stacks[0].base_branch,
            Some("coworker-feat".to_string()),
        );
        assert_eq!(graph.stacks[0].segments.len(), 1);
        assert_eq!(graph.stacks[0].segments[0].bookmarks[0].name, "feature");
    }

    #[test]
    fn test_stack_without_foreign_base() {
        // Normal stack: trunk -> commit_a (bookmarked "feature")
        let jj = StubJj {
            bookmarks: vec![make_bookmark("feature", "commit_a", "change_a")],
            log_entries: HashMap::from([(
                "commit_a".to_string(),
                vec![make_log_entry(
                    "commit_a", "change_a", vec!["trunk"], vec!["feature"],
                )],
            )]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.stacks.len(), 1);
        assert!(graph.stacks[0].base_branch.is_none());
    }

    #[test]
    fn test_multi_segment_stack_with_foreign_base() {
        // trunk -> coworker_commit (foreign) -> commit_a (auth) -> commit_b (profile)
        // Both auth and profile should be in the same stack with base_branch set.
        let jj = StubJj {
            bookmarks: vec![
                make_bookmark("auth", "commit_a", "change_a"),
                make_bookmark("profile", "commit_b", "change_b"),
            ],
            log_entries: HashMap::from([
                (
                    "commit_a".to_string(),
                    vec![
                        make_log_entry("commit_a", "change_a", vec!["coworker_c"], vec!["auth"]),
                        make_log_entry_with_remote_bookmarks(
                            "coworker_c", "coworker_ch", vec!["trunk"],
                            vec![], vec!["coworker-feat@origin"],
                        ),
                    ],
                ),
                (
                    "commit_b".to_string(),
                    vec![
                        make_log_entry("commit_b", "change_b", vec!["commit_a"], vec!["profile"]),
                        make_log_entry("commit_a", "change_a", vec!["coworker_c"], vec!["auth"]),
                        make_log_entry_with_remote_bookmarks(
                            "coworker_c", "coworker_ch", vec!["trunk"],
                            vec![], vec!["coworker-feat@origin"],
                        ),
                    ],
                ),
            ]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.stacks.len(), 1);
        assert_eq!(
            graph.stacks[0].base_branch,
            Some("coworker-feat".to_string()),
            "multi-segment stack should propagate foreign base"
        );
        assert_eq!(graph.stacks[0].segments.len(), 2);
        assert_eq!(graph.stacks[0].segments[0].bookmarks[0].name, "auth");
        assert_eq!(graph.stacks[0].segments[1].bookmarks[0].name, "profile");
    }

    #[test]
    fn test_diamond_merge_included_in_stack() {
        // Diamond: trunk -> B, trunk -> C, B+C -> D (merge)
        // D follows through B, C gets its own stack
        let jj = StubJj {
            bookmarks: vec![
                make_bookmark("B", "commit_b", "change_b"),
                make_bookmark("C", "commit_c", "change_c"),
                make_bookmark("D", "commit_d", "change_d"),
            ],
            log_entries: HashMap::from([
                (
                    "commit_b".to_string(),
                    vec![make_log_entry("commit_b", "change_b", vec!["trunk"], vec!["B"])],
                ),
                (
                    "commit_c".to_string(),
                    vec![make_log_entry("commit_c", "change_c", vec!["trunk"], vec!["C"])],
                ),
                (
                    "commit_d".to_string(),
                    vec![
                        make_log_entry("commit_d", "change_d", vec!["commit_b", "commit_c"], vec!["D"]),
                        make_log_entry("commit_b", "change_b", vec!["trunk"], vec!["B"]),
                        make_log_entry("commit_c", "change_c", vec!["trunk"], vec!["C"]),
                    ],
                ),
            ]),
        };

        let graph = build_change_graph(&jj).unwrap();
        // All 3 bookmarks should be in stacks
        let all_stack_names: HashSet<String> = graph
            .stacks
            .iter()
            .flat_map(|s| s.segments.iter().flat_map(|seg| seg.bookmarks.iter().map(|b| b.name.clone())))
            .collect();
        assert!(all_stack_names.contains("B"));
        assert!(all_stack_names.contains("C"));
        assert!(all_stack_names.contains("D"));
    }

    #[test]
    fn test_merge_skipped_arm_forms_own_stack() {
        // D (merge of B and C) follows B. C should form its own independent stack.
        let jj = StubJj {
            bookmarks: vec![
                make_bookmark("D", "commit_d", "change_d"),
                make_bookmark("B", "commit_b", "change_b"),
                make_bookmark("C", "commit_c", "change_c"),
            ],
            log_entries: HashMap::from([
                (
                    "commit_d".to_string(),
                    vec![
                        make_log_entry("commit_d", "change_d", vec!["commit_b", "commit_c"], vec!["D"]),
                        make_log_entry("commit_b", "change_b", vec!["trunk"], vec!["B"]),
                        make_log_entry("commit_c", "change_c", vec!["trunk"], vec!["C"]),
                    ],
                ),
                (
                    "commit_b".to_string(),
                    vec![make_log_entry("commit_b", "change_b", vec!["trunk"], vec!["B"])],
                ),
                (
                    "commit_c".to_string(),
                    vec![make_log_entry("commit_c", "change_c", vec!["trunk"], vec!["C"])],
                ),
            ]),
        };

        let graph = build_change_graph(&jj).unwrap();
        // D+B in one stack, C in another
        let c_stack = graph.stacks.iter().find(|s|
            s.segments.iter().any(|seg| seg.bookmarks.iter().any(|b| b.name == "C"))
        );
        assert!(c_stack.is_some(), "C should have its own stack");
    }

    #[test]
    fn test_merge_source_names_on_segment() {
        // Merge D (parents B, C) — verify segment carries merge_source_names
        let jj = StubJj {
            bookmarks: vec![
                make_bookmark("D", "commit_d", "change_d"),
                make_bookmark("B", "commit_b", "change_b"),
                make_bookmark("C", "commit_c", "change_c"),
            ],
            log_entries: HashMap::from([
                (
                    "commit_d".to_string(),
                    vec![
                        make_log_entry("commit_d", "change_d", vec!["commit_b", "commit_c"], vec!["D"]),
                        make_log_entry("commit_b", "change_b", vec!["trunk"], vec!["B"]),
                        make_log_entry("commit_c", "change_c", vec!["trunk"], vec!["C"]),
                    ],
                ),
                (
                    "commit_b".to_string(),
                    vec![make_log_entry("commit_b", "change_b", vec!["trunk"], vec!["B"])],
                ),
                (
                    "commit_c".to_string(),
                    vec![make_log_entry("commit_c", "change_c", vec!["trunk"], vec!["C"])],
                ),
            ]),
        };

        let graph = build_change_graph(&jj).unwrap();
        let d_segment = graph.stacks.iter()
            .flat_map(|s| &s.segments)
            .find(|seg| seg.bookmarks.iter().any(|b| b.name == "D"))
            .expect("D should be in a stack");
        assert_eq!(d_segment.merge_source_names, vec!["C"]);
    }

    #[test]
    fn test_bookmark_above_merge_included() {
        // trunk -> merge_change(parents: p1, p2) -> linear_change (bookmark "top")
        // "top" should now be included, following through p1.
        let jj = StubJj {
            bookmarks: vec![make_bookmark("top", "commit_top", "change_top")],
            log_entries: HashMap::from([(
                "commit_top".to_string(),
                vec![
                    make_log_entry("commit_top", "change_top", vec!["commit_m"], vec!["top"]),
                    make_log_entry("commit_m", "change_m", vec!["p1", "p2"], vec![]),
                ],
            )]),
        };

        let graph = build_change_graph(&jj).unwrap();
        assert_eq!(graph.stacks.len(), 1);
        assert_eq!(graph.stacks[0].segments.len(), 1);
        assert_eq!(graph.stacks[0].segments[0].bookmarks[0].name, "top");
    }
}
