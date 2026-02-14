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
    pub excluded_bookmarks: HashSet<String>,
    pub excluded_bookmark_count: usize,
}

/// Build the change graph from the current jj repo state.
pub fn build_change_graph(jj: &dyn Jj) -> Result<ChangeGraph> {
    let bookmarks = jj.get_my_bookmarks()?;

    let mut all_bookmarks: HashMap<String, Bookmark> = HashMap::new();
    let mut bookmark_to_change_id: HashMap<String, String> = HashMap::new();
    let mut adjacency_list: HashMap<String, String> = HashMap::new();
    let mut change_id_to_segment: HashMap<String, Vec<LogEntry>> = HashMap::new();
    let mut tainted: HashSet<String> = HashSet::new();
    let mut fully_collected: HashSet<String> = HashSet::new();
    let mut excluded_names: HashSet<String> = HashSet::new();
    let mut excluded_count = 0;

    for bookmark in &bookmarks {
        all_bookmarks.insert(bookmark.name.clone(), bookmark.clone());
        bookmark_to_change_id.insert(bookmark.name.clone(), bookmark.change_id.clone());
    }

    // Traverse each bookmark toward trunk, discovering segments
    for bookmark in &bookmarks {
        if tainted.contains(&bookmark.change_id) {
            excluded_names.insert(bookmark.name.clone());
            excluded_count += 1;
            continue;
        }

        let result = traversal::traverse_and_discover_segments(
            jj,
            &bookmark.commit_id,
            &tainted,
            &fully_collected,
            &all_bookmarks,
        )?;

        if result.has_merge {
            tainted.extend(result.seen_change_ids);
            excluded_names.insert(bookmark.name.clone());
            excluded_count += 1;
            continue;
        }

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
                .filter(|b| !tainted.contains(&b.change_id))
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
    );

    Ok(ChangeGraph {
        bookmarks: all_bookmarks,
        bookmark_to_change_id,
        adjacency_list,
        change_id_to_segment,
        stack_leafs,
        stack_roots,
        stacks,
        excluded_bookmarks: excluded_names,
        excluded_bookmark_count: excluded_count,
    })
}

fn build_stacks(
    leafs: &HashSet<String>,
    adjacency_list: &HashMap<String, String>,
    change_id_to_segment: &HashMap<String, Vec<LogEntry>>,
    bookmarks: &HashMap<String, Bookmark>,
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
                Some(BookmarkSegment {
                    bookmarks: segment_bookmarks,
                    changes,
                })
            })
            .collect();

        if !segments.is_empty() {
            stacks.push(BranchStack { segments });
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
        assert_eq!(graph.excluded_bookmark_count, 0);
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
        assert_eq!(graph.excluded_bookmark_count, 0);
        assert!(!graph.stacks.is_empty());

        // Verify the stack has both segments in order
        let stack = &graph.stacks[0];
        assert_eq!(stack.segments.len(), 2);
        assert_eq!(stack.segments[0].bookmarks[0].name, "auth");
        assert_eq!(stack.segments[1].bookmarks[0].name, "profile");
    }

    #[test]
    fn test_merge_commit_excludes_bookmark() {
        // A bookmark whose ancestry contains a merge commit should be excluded.
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
        assert_eq!(graph.excluded_bookmark_count, 1);
        assert!(graph.stacks.is_empty());
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
        assert_eq!(graph.excluded_bookmark_count, 0);
        // Each bookmark is its own stack (no adjacency relationship)
        assert_eq!(graph.stacks.len(), 2);
    }
}
