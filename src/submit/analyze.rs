use std::collections::HashSet;

use anyhow::Result;

use crate::graph::ChangeGraph;
use crate::graph::change_graph::find_stack_with_bookmark;
use crate::jj::Jj;
use crate::jj::types::{BookmarkSegment, BranchStack};

/// The result of analyzing which segments need to be submitted.
#[derive(Debug)]
pub struct SubmissionAnalysis {
    pub target_bookmark: String,
    pub relevant_segments: Vec<BookmarkSegment>,
    /// If the stack is based on a foreign branch (not trunk), this is the branch name.
    pub base_branch: Option<String>,
}

/// Find the stack containing `target_bookmark` and return all segments
/// from trunk up to and including that bookmark.
pub fn analyze_submission_graph(
    graph: &ChangeGraph,
    target_bookmark: &str,
) -> Result<SubmissionAnalysis> {
    if !graph.bookmarks.contains_key(target_bookmark) {
        anyhow::bail!(
            "bookmark '{}' not found. Is it created with `jj bookmark set`?",
            target_bookmark
        );
    }

    // Match on the bookmark NAME. Resolving to a change id first and comparing
    // that would pick the wrong segment when the change is divergent: two
    // bookmarks on different commits share one change id, so the first segment
    // carrying either of them wins. Names are unique; change ids are not.
    for stack in &graph.stacks {
        let target_idx = stack
            .segments
            .iter()
            .position(|seg| seg.bookmarks.iter().any(|b| b.name == target_bookmark));

        if let Some(idx) = target_idx {
            let relevant = stack.segments[..=idx].to_vec();
            return Ok(SubmissionAnalysis {
                target_bookmark: target_bookmark.to_string(),
                relevant_segments: relevant,
                base_branch: stack.base_branch.clone(),
            });
        }
    }

    anyhow::bail!(
        "bookmark '{}' not found in any stack. Run `jjpr` to see your stacks.",
        target_bookmark
    )
}

/// Find the stack the working copy belongs to.
///
/// Queries `trunk()..@` and returns the first stack whose segments overlap
/// the working copy's ancestry. Returns `None` if no stack overlaps.
pub fn infer_target_stack<'a>(
    graph: &'a ChangeGraph,
    jj: &dyn Jj,
) -> Result<Option<&'a BranchStack>> {
    Ok(overlapping_stacks(graph, jj)?.into_iter().next())
}

/// Every stack whose ancestry the working copy touches. Usually one, but a
/// merge working copy (a diamond) reaches into both parents' stacks.
fn overlapping_stacks<'a>(graph: &'a ChangeGraph, jj: &dyn Jj) -> Result<Vec<&'a BranchStack>> {
    let wc_commit_id = jj.get_working_copy_commit_id()?;
    let wc_ancestry = jj.get_changes_to_commit(&wc_commit_id)?;
    // Commit ids, not change ids: a divergent change puts one id on two commits,
    // and matching by it would pull in a stack the working copy does not touch.
    let wc_commit_ids: HashSet<String> = wc_ancestry.iter().map(|e| e.commit_id.clone()).collect();

    Ok(graph
        .stacks
        .iter()
        .filter(|stack| {
            stack.segments.iter().any(|seg| {
                seg.bookmarks
                    .iter()
                    .any(|b| wc_commit_ids.contains(&b.commit_id))
            })
        })
        .collect())
}

fn stack_contains_mine(stack: &BranchStack, my_names: &HashSet<String>) -> bool {
    stack
        .segments
        .iter()
        .any(|seg| seg.bookmarks.iter().any(|b| my_names.contains(&b.name)))
}

/// Infer the target bookmark from the working copy's position in the graph.
///
/// Returns the leaf (topmost) bookmark of the inferred stack, or `None`.
/// Submit/merge/watch use this when the user omits the positional bookmark.
pub fn infer_target_bookmark(graph: &ChangeGraph, jj: &dyn Jj) -> Result<Option<String>> {
    let Some(stack) = infer_target_stack(graph, jj)? else {
        return Ok(None);
    };
    let leaf = stack
        .segments
        .last()
        .and_then(|s| s.bookmarks.first())
        .ok_or_else(|| anyhow::anyhow!("stack has no bookmarks"))?;
    Ok(Some(leaf.name.clone()))
}

/// Outcome of scoping the `status` view to a subset of stacks.
#[derive(Debug)]
pub enum StackScope<'a> {
    /// Display these stacks. Always non-empty when returned.
    Show(Vec<&'a BranchStack>),
    /// `--all` was not passed and no target bookmark could be resolved
    /// (none provided and inference returned `None`).
    NoTarget,
    /// A target bookmark was provided but no stack contains it.
    Unknown(String),
}

/// Decide which stacks `status` should display.
///
/// - `all = true`: every stack in the graph.
/// - `bookmark = Some(name)`: the stack containing that bookmark, or
///   `Unknown(name)` if none.
/// - both `None`: the stack inferred from the working copy's ancestry,
///   or `NoTarget` if none.
///
/// Walks `graph.stacks` at most once: either via `find_stack_with_bookmark`
/// (user-supplied target) or via `infer_target_stack` (working-copy target).
pub fn select_stacks_to_show<'a>(
    graph: &'a ChangeGraph,
    bookmark: Option<&str>,
    all: bool,
    jj: &dyn Jj,
    my_names: &HashSet<String>,
) -> Result<StackScope<'a>> {
    if all {
        return Ok(StackScope::Show(graph.stacks.iter().collect()));
    }
    if let Some(bookmark) = bookmark {
        return Ok(match find_stack_with_bookmark(graph, bookmark) {
            Some(stack) => StackScope::Show(vec![stack]),
            None => StackScope::Unknown(bookmark.to_string()),
        });
    }
    // When the working copy is a merge (a diamond), it overlaps both parents'
    // stacks. Prefer one that contains YOUR work, so status shows your arm
    // rather than arbitrarily picking a coworker's and claiming "nothing of
    // yours". Falls back to the first overlap when none is yours.
    let overlapping = overlapping_stacks(graph, jj)?;
    let chosen = overlapping
        .iter()
        .copied()
        .find(|s| stack_contains_mine(s, my_names))
        .or_else(|| overlapping.first().copied());
    Ok(match chosen {
        Some(stack) => StackScope::Show(vec![stack]),
        None => StackScope::NoTarget,
    })
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
                conflict: false,
                empty: false,
            }],
            merge_source_names: vec![],
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
            commit_id_to_segment: HashMap::new(),
            stack_leafs: HashSet::new(),
            stack_roots: HashSet::new(),
            stacks: vec![BranchStack {
                segments: segments.clone(),
                base_branch: None,
            }],
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

    struct StubJj {
        wc_commit_id: String,
        branch_changes: Vec<LogEntry>,
    }

    impl crate::jj::Jj for StubJj {
        fn git_fetch(&self) -> Result<()> {
            Ok(())
        }
        fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
            Ok(vec![])
        }
        fn get_changes_to_commit(&self, _to: &str) -> Result<Vec<LogEntry>> {
            Ok(self.branch_changes.clone())
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
            Ok(self.wc_commit_id.clone())
        }
        fn rebase_onto(&self, _source: &str, _dest: &str) -> Result<()> {
            unimplemented!()
        }
        fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> {
            unimplemented!()
        }
        fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
            Ok(vec!["dummy_commit_id".to_string()])
        }
        fn is_conflicted(&self, _revset: &str) -> Result<bool> {
            Ok(false)
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
            conflict: false,
            empty: false,
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
        assert_eq!(result.as_deref(), Some("profile"));
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
        assert_eq!(result.as_deref(), Some("profile"));
    }

    #[test]
    fn test_infer_bookmark_no_bookmarks() {
        let graph = make_graph(vec![make_segment("feature", "ch1")]);
        let jj = StubJj {
            wc_commit_id: "commit_unrelated".to_string(),
            branch_changes: vec![make_log_entry("ch_other")],
        };

        let result = infer_target_bookmark(&graph, &jj).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_infer_bookmark_empty_graph() {
        let graph = ChangeGraph {
            bookmarks: HashMap::new(),
            bookmark_to_change_id: HashMap::new(),
            adjacency_list: HashMap::new(),
            commit_id_to_segment: HashMap::new(),
            stack_leafs: HashSet::new(),
            stack_roots: HashSet::new(),
            stacks: vec![],
        };
        let jj = StubJj {
            wc_commit_id: "commit_wc".to_string(),
            branch_changes: vec![make_log_entry("ch_wc")],
        };

        let result = infer_target_bookmark(&graph, &jj).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_analyze_propagates_base_branch() {
        let segments = vec![make_segment("auth", "ch1"), make_segment("profile", "ch2")];
        let mut graph = make_graph(segments);
        graph.stacks[0].base_branch = Some("coworker-feat".to_string());

        let analysis = analyze_submission_graph(&graph, "profile").unwrap();
        assert_eq!(analysis.base_branch, Some("coworker-feat".to_string()));
    }

    #[test]
    fn test_analyze_no_base_branch_when_none() {
        let segments = vec![make_segment("feature", "ch1")];
        let graph = make_graph(segments);

        let analysis = analyze_submission_graph(&graph, "feature").unwrap();
        assert!(analysis.base_branch.is_none());
    }

    fn make_graph_multi(stacks: Vec<Vec<(&str, &str)>>) -> ChangeGraph {
        let stacks: Vec<BranchStack> = stacks
            .into_iter()
            .map(|seg_specs| BranchStack {
                segments: seg_specs
                    .into_iter()
                    .map(|(name, ch)| make_segment(name, ch))
                    .collect(),
                base_branch: None,
            })
            .collect();
        ChangeGraph {
            bookmarks: HashMap::new(),
            bookmark_to_change_id: HashMap::new(),
            adjacency_list: HashMap::new(),
            commit_id_to_segment: HashMap::new(),
            stack_leafs: HashSet::new(),
            stack_roots: HashSet::new(),
            stacks,
        }
    }

    #[test]
    fn select_stacks_all_returns_every_stack() {
        let graph = make_graph_multi(vec![vec![("auth", "ch1")], vec![("payments", "ch2")]]);
        let jj = StubJj {
            wc_commit_id: "wc".to_string(),
            branch_changes: vec![],
        };

        match select_stacks_to_show(&graph, None, true, &jj, &HashSet::new()).unwrap() {
            StackScope::Show(stacks) => assert_eq!(stacks.len(), 2),
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn select_stacks_explicit_bookmark_returns_one_stack() {
        let graph = make_graph_multi(vec![
            vec![("auth", "ch1")],
            vec![("payments", "ch2"), ("checkout", "ch3")],
        ]);
        let jj = StubJj {
            wc_commit_id: "wc".to_string(),
            branch_changes: vec![],
        };

        match select_stacks_to_show(&graph, Some("checkout"), false, &jj, &HashSet::new()).unwrap()
        {
            StackScope::Show(stacks) => {
                assert_eq!(stacks.len(), 1);
                assert_eq!(stacks[0].segments[0].bookmarks[0].name, "payments");
            }
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn select_stacks_unknown_bookmark_returns_unknown() {
        let graph = make_graph_multi(vec![vec![("auth", "ch1")]]);
        let jj = StubJj {
            wc_commit_id: "wc".to_string(),
            branch_changes: vec![],
        };

        match select_stacks_to_show(&graph, Some("missing"), false, &jj, &HashSet::new()).unwrap() {
            StackScope::Unknown(name) => assert_eq!(name, "missing"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn select_stacks_inferred_from_working_copy() {
        let graph = make_graph_multi(vec![
            vec![("auth", "ch1")],
            vec![("payments", "ch2"), ("checkout", "ch3")],
        ]);
        // wc ancestry includes ch2 → matches the second stack
        let jj = StubJj {
            wc_commit_id: "commit_ch2".to_string(),
            branch_changes: vec![make_log_entry("ch2")],
        };

        match select_stacks_to_show(&graph, None, false, &jj, &HashSet::new()).unwrap() {
            StackScope::Show(stacks) => {
                assert_eq!(stacks.len(), 1);
                assert_eq!(stacks[0].segments[0].bookmarks[0].name, "payments");
            }
            other => panic!("expected Show, got {other:?}"),
        }
    }

    #[test]
    fn select_stacks_no_target_when_inference_fails() {
        let graph = make_graph_multi(vec![vec![("auth", "ch1")]]);
        let jj = StubJj {
            wc_commit_id: "wc".to_string(),
            branch_changes: vec![make_log_entry("ch_other")],
        };

        match select_stacks_to_show(&graph, None, false, &jj, &HashSet::new()).unwrap() {
            StackScope::NoTarget => {}
            other => panic!("expected NoTarget, got {other:?}"),
        }
    }

    #[test]
    fn select_stacks_prefers_your_arm_of_a_diamond() {
        // Working copy is a merge whose ancestry reaches BOTH a coworker's stack
        // (first in graph order) and yours.
        let graph = make_graph_multi(vec![
            vec![("coworker-feat", "ch_c")],
            vec![("my-feat", "ch_m")],
        ]);
        let jj = StubJj {
            wc_commit_id: "merge".to_string(),
            branch_changes: vec![make_log_entry("ch_c"), make_log_entry("ch_m")],
        };

        // With no ownership info, inference falls back to the first overlap.
        match select_stacks_to_show(&graph, None, false, &jj, &HashSet::new()).unwrap() {
            StackScope::Show(stacks) => {
                assert_eq!(stacks[0].segments[0].bookmarks[0].name, "coworker-feat");
            }
            other => panic!("expected Show, got {other:?}"),
        }

        // Knowing my-feat is yours, status must pick your arm — not arbitrarily
        // land on the coworker's and hide your work behind "nothing of yours".
        let mine: HashSet<String> = ["my-feat".to_string()].into_iter().collect();
        match select_stacks_to_show(&graph, None, false, &jj, &mine).unwrap() {
            StackScope::Show(stacks) => {
                assert_eq!(stacks[0].segments[0].bookmarks[0].name, "my-feat");
            }
            other => panic!("expected Show, got {other:?}"),
        }
    }
}
