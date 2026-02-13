use anyhow::Result;

use crate::graph::ChangeGraph;
use crate::jj::types::BookmarkSegment;

/// The result of analyzing which segments need to be submitted.
#[derive(Debug)]
pub struct SubmissionAnalysis {
    pub target_bookmark: String,
    pub relevant_segments: Vec<BookmarkSegment>,
}

/// Find the stack containing `target_bookmark` and return all segments
/// from trunk up to and including that bookmark.
pub fn analyze_submission_graph(
    graph: &ChangeGraph,
    target_bookmark: &str,
) -> Result<SubmissionAnalysis> {
    let target_change_id = graph
        .bookmark_to_change_id
        .get(target_bookmark)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "bookmark '{}' not found. Is it created with `jj bookmark set`?",
                target_bookmark
            )
        })?;

    // Find which stack contains this bookmark
    for stack in &graph.stacks {
        let target_idx = stack
            .segments
            .iter()
            .position(|seg| seg.bookmarks.iter().any(|b| b.change_id == *target_change_id));

        if let Some(idx) = target_idx {
            let relevant = stack.segments[..=idx].to_vec();
            return Ok(SubmissionAnalysis {
                target_bookmark: target_bookmark.to_string(),
                relevant_segments: relevant,
            });
        }
    }

    anyhow::bail!(
        "bookmark '{}' not found in any stack. Run `stk` to see your stacks.",
        target_bookmark
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::types::{Bookmark, BranchStack, LogEntry};
    use std::collections::{HashMap, HashSet};

    fn make_segment(bookmark_name: &str, change_id: &str) -> BookmarkSegment {
        BookmarkSegment {
            bookmarks: vec![Bookmark {
                name: bookmark_name.to_string(),
                commit_id: format!("commit_{change_id}"),
                change_id: change_id.to_string(),
                has_remote: false,
                is_synced: false,
            }],
            changes: vec![LogEntry {
                commit_id: format!("commit_{change_id}"),
                change_id: change_id.to_string(),
                author_name: "Test".to_string(),
                author_email: "test@test.com".to_string(),
                description: bookmark_name.to_string(),
                description_first_line: bookmark_name.to_string(),
                parents: vec![],
                local_bookmarks: vec![bookmark_name.to_string()],
                remote_bookmarks: vec![],
                is_working_copy: false,
            }],
        }
    }

    fn make_graph(segments: Vec<BookmarkSegment>) -> ChangeGraph {
        let mut bookmarks = HashMap::new();
        let mut bookmark_to_change_id = HashMap::new();
        for seg in &segments {
            for b in &seg.bookmarks {
                bookmarks.insert(b.name.clone(), b.clone());
                bookmark_to_change_id.insert(b.name.clone(), b.change_id.clone());
            }
        }

        ChangeGraph {
            bookmarks,
            bookmark_to_change_id,
            adjacency_list: HashMap::new(),
            change_id_to_segment: HashMap::new(),
            stack_leafs: HashSet::new(),
            stack_roots: HashSet::new(),
            stacks: vec![BranchStack {
                segments: segments.clone(),
            }],
            excluded_bookmark_count: 0,
        }
    }

    #[test]
    fn test_analyze_finds_target_segment() {
        let segments = vec![
            make_segment("auth", "ch1"),
            make_segment("profile", "ch2"),
            make_segment("settings", "ch3"),
        ];
        let graph = make_graph(segments);

        let analysis = analyze_submission_graph(&graph, "profile").unwrap();
        assert_eq!(analysis.target_bookmark, "profile");
        assert_eq!(analysis.relevant_segments.len(), 2);
        assert_eq!(analysis.relevant_segments[0].bookmarks[0].name, "auth");
        assert_eq!(analysis.relevant_segments[1].bookmarks[0].name, "profile");
    }

    #[test]
    fn test_analyze_includes_all_downstack() {
        let segments = vec![
            make_segment("base", "ch1"),
            make_segment("middle", "ch2"),
            make_segment("top", "ch3"),
        ];
        let graph = make_graph(segments);

        let analysis = analyze_submission_graph(&graph, "top").unwrap();
        assert_eq!(analysis.relevant_segments.len(), 3);
    }

    #[test]
    fn test_analyze_single_bookmark() {
        let segments = vec![make_segment("feature", "ch1")];
        let graph = make_graph(segments);

        let analysis = analyze_submission_graph(&graph, "feature").unwrap();
        assert_eq!(analysis.relevant_segments.len(), 1);
    }

    #[test]
    fn test_analyze_unknown_bookmark() {
        let graph = make_graph(vec![make_segment("feature", "ch1")]);
        let err = analyze_submission_graph(&graph, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }
}
