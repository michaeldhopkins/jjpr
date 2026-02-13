use std::collections::HashSet;

use anyhow::Result;

use crate::graph::ChangeGraph;
use crate::jj::types::BookmarkSegment;
use crate::jj::Jj;

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
    if graph.excluded_bookmarks.contains(target_bookmark) {
        anyhow::bail!(
            "bookmark '{}' was excluded because it has merge commits in its ancestry. \
             Rebase to a linear history to submit.",
            target_bookmark
        );
    }

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
        "bookmark '{}' not found in any stack. Run `jjpr` to see your stacks.",
        target_bookmark
    )
}

/// Infer the target bookmark from the working copy's position in the graph.
///
/// Queries `trunk()..@` to find which stack the working copy belongs to,
/// then returns the leaf (topmost) bookmark of that stack.
pub fn infer_target_bookmark(graph: &ChangeGraph, jj: &dyn Jj) -> Result<String> {
    let wc_commit_id = jj.get_working_copy_commit_id()?;
    let wc_ancestry = jj.get_branch_changes(&wc_commit_id)?;
    let wc_change_ids: HashSet<String> = wc_ancestry.iter()
        .map(|e| e.change_id.clone()).collect();

    for stack in &graph.stacks {
        let overlaps = stack.segments.iter().any(|seg|
            seg.bookmarks.iter().any(|b| wc_change_ids.contains(&b.change_id))
        );
        if overlaps {
            // Return the leaf bookmark (last segment's first bookmark)
            let leaf = stack.segments.last()
                .and_then(|s| s.bookmarks.first())
                .ok_or_else(|| anyhow::anyhow!("stack has no bookmarks"))?;
            return Ok(leaf.name.clone());
        }
    }

    anyhow::bail!(
        "no bookmark found in the working copy's ancestry. \
         Set a bookmark with `jj bookmark set <name>` or specify one: \
         `jjpr submit <bookmark>`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::types::{Bookmark, BranchStack, GitRemote, LogEntry};
    use std::collections::HashMap;

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
            excluded_bookmarks: HashSet::new(),
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

    #[test]
    fn test_analyze_excluded_bookmark_gives_specific_error() {
        let mut graph = make_graph(vec![make_segment("feature", "ch1")]);
        graph.excluded_bookmarks.insert("broken".to_string());
        let err = analyze_submission_graph(&graph, "broken").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("merge commits"), "should mention merge commits: {msg}");
        assert!(msg.contains("broken"), "should mention bookmark name: {msg}");
    }

    struct StubJj {
        wc_commit_id: String,
        branch_changes: Vec<LogEntry>,
    }

    impl crate::jj::Jj for StubJj {
        fn git_fetch(&self) -> Result<()> { Ok(()) }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> { Ok(vec![]) }
        fn get_branch_changes(&self, _to: &str) -> Result<Vec<LogEntry>> {
            Ok(self.branch_changes.clone())
        }
        fn get_git_remotes(&self) -> Result<Vec<GitRemote>> { Ok(vec![]) }
        fn get_default_branch(&self) -> Result<String> { Ok("main".to_string()) }
        fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> { Ok(()) }
        fn get_working_copy_commit_id(&self) -> Result<String> {
            Ok(self.wc_commit_id.clone())
        }
    }

    fn make_log_entry(change_id: &str) -> LogEntry {
        LogEntry {
            commit_id: format!("commit_{change_id}"),
            change_id: change_id.to_string(),
            author_name: "Test".to_string(),
            author_email: "test@test.com".to_string(),
            description: "test".to_string(),
            description_first_line: "test".to_string(),
            parents: vec![],
            local_bookmarks: vec![],
            remote_bookmarks: vec![],
            is_working_copy: false,
        }
    }

    #[test]
    fn test_infer_bookmark_wc_at_bookmark() {
        let graph = make_graph(vec![
            make_segment("auth", "ch1"),
            make_segment("profile", "ch2"),
        ]);
        let jj = StubJj {
            wc_commit_id: "commit_ch2".to_string(),
            branch_changes: vec![make_log_entry("ch2"), make_log_entry("ch1")],
        };

        let result = infer_target_bookmark(&graph, &jj).unwrap();
        assert_eq!(result, "profile");
    }

    #[test]
    fn test_infer_bookmark_wc_above_bookmarks() {
        let graph = make_graph(vec![
            make_segment("auth", "ch1"),
            make_segment("profile", "ch2"),
        ]);
        // Working copy is above the stack but its ancestry includes bookmarked changes
        let jj = StubJj {
            wc_commit_id: "commit_ch3".to_string(),
            branch_changes: vec![
                make_log_entry("ch3"),
                make_log_entry("ch2"),
                make_log_entry("ch1"),
            ],
        };

        let result = infer_target_bookmark(&graph, &jj).unwrap();
        assert_eq!(result, "profile");
    }

    #[test]
    fn test_infer_bookmark_no_bookmarks() {
        let graph = make_graph(vec![make_segment("feature", "ch1")]);
        let jj = StubJj {
            wc_commit_id: "commit_unrelated".to_string(),
            branch_changes: vec![make_log_entry("ch_other")],
        };

        let err = infer_target_bookmark(&graph, &jj).unwrap_err();
        assert!(err.to_string().contains("no bookmark found"));
    }

    #[test]
    fn test_infer_bookmark_empty_graph() {
        let graph = ChangeGraph {
            bookmarks: HashMap::new(),
            bookmark_to_change_id: HashMap::new(),
            adjacency_list: HashMap::new(),
            change_id_to_segment: HashMap::new(),
            stack_leafs: HashSet::new(),
            stack_roots: HashSet::new(),
            stacks: vec![],
            excluded_bookmarks: HashSet::new(),
            excluded_bookmark_count: 0,
        };
        let jj = StubJj {
            wc_commit_id: "commit_wc".to_string(),
            branch_changes: vec![make_log_entry("ch_wc")],
        };

        let err = infer_target_bookmark(&graph, &jj).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no bookmark found"), "got: {msg}");
        assert!(msg.contains("jj bookmark set"), "should suggest setting a bookmark: {msg}");
    }
}
