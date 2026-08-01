#![no_main]

//! GRAPH-SOUNDNESS target: what must hold about a stack jjpr built from jj's output.
//!
//! This is the composition of the other parsing target with the logic above it. The
//! stub `Jj` answers `get_my_bookmarks` and `get_changes_to_commit` from *fuzzed jj
//! output*, so a single input drives the whole path jjpr actually takes at startup:
//! subprocess bytes → `parse_bookmark_output` / `parse_log_output` → segment
//! traversal → `ChangeGraph`. `jj_output` asks only "did the parse survive"; this
//! asks "is what it built coherent".
//!
//! The invariant that matters is ACYCLICITY. `adjacency_list` maps child change → its
//! parent, and everything downstream — traversal toward trunk, submit's ordering,
//! merge walking the stack — follows those links assuming they terminate. jjpr does
//! not build that map from a graph library with cycle checks; it builds it by reading
//! lines of another program's output. A cycle would not be caught by any type: it
//! would hang `jjpr status` on a repo the user cannot easily un-break, and it would
//! take a very specific jj output to produce, which is exactly what a fuzzer is for
//! and a hand-written test is not.
//!
//! A hang inside the traversal shows up as a libFuzzer timeout artifact rather than
//! an assertion, and is a genuine finding of the same bug — the assertion below
//! catches the case where the cycle is *built* but not yet walked.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use libfuzzer_sys::fuzz_target;

use jjpr::graph::change_graph::build_change_graph;
use jjpr::jj::templates::{parse_bookmark_output, parse_log_output};
use jjpr::jj::types::{Bookmark, GitRemote, LogEntry};
use jjpr::jj::Jj;

/// Answers the two read calls from fuzzed `jj` output; everything jjpr would use to
/// MUTATE a repo is inert, so a fuzz run can never be mistaken for one that writes.
struct FuzzedJj {
    bookmarks: Vec<Bookmark>,
    changes: Vec<LogEntry>,
}

impl Jj for FuzzedJj {
    fn get_my_bookmarks(&self) -> Result<Vec<Bookmark>> {
        Ok(self.bookmarks.clone())
    }
    /// Real jj answers `trunk()..<to>` — the ancestry of *that* commit, newest
    /// first — so this walks `parents[0]` from `to_commit_id` instead of handing
    /// back the whole log.
    ///
    /// Returning every entry regardless of the argument, which this did first,
    /// models a jj that cannot exist: two bookmarks would report identical
    /// ancestry, and `build_change_graph` would link them both ways and trip the
    /// acyclicity assertion below. That is a bug in the stub, not in jjpr, and it
    /// cost a real diagnosis before the stub was made faithful. **A stub that
    /// ignores its arguments manufactures findings.**
    ///
    /// The `seen` guard is for the fuzzer, not for jj: the input log can name a
    /// parent cycle, and the stub must not spin on it while modelling a program
    /// that never would.
    fn get_changes_to_commit(&self, to_commit_id: &str) -> Result<Vec<LogEntry>> {
        let by_commit: HashMap<&str, &LogEntry> = self
            .changes
            .iter()
            .map(|e| (e.commit_id.as_str(), e))
            .collect();

        let mut chain = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut cursor = to_commit_id;
        while let Some(entry) = by_commit.get(cursor) {
            if !seen.insert(cursor) {
                break;
            }
            chain.push((*entry).clone());
            match entry.parents.first() {
                Some(parent) => cursor = parent.as_str(),
                None => break,
            }
        }
        Ok(chain)
    }
    fn get_default_branch(&self) -> Result<String> {
        Ok("main".to_string())
    }
    fn get_working_copy_commit_id(&self) -> Result<String> {
        Ok("wc".to_string())
    }
    fn get_git_remotes(&self) -> Result<Vec<GitRemote>> {
        Ok(vec![])
    }
    fn git_fetch(&self) -> Result<()> {
        Ok(())
    }
    fn push_bookmark(&self, _name: &str, _remote: &str) -> Result<()> {
        unreachable!("a fuzz run must never push")
    }
    fn rebase_onto(&self, _source: &str, _destination: &str) -> Result<()> {
        unreachable!("a fuzz run must never rebase")
    }
    fn merge_into(&self, _bookmark: &str, _dest: &str) -> Result<()> {
        unreachable!("a fuzz run must never merge")
    }
    fn resolve_change_id(&self, _change_id: &str) -> Result<Vec<String>> {
        Ok(vec![])
    }
    fn is_conflicted(&self, _revset: &str) -> Result<bool> {
        Ok(false)
    }
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);

    // Split the input so one half shapes the bookmarks and the other the log; a
    // single blob would make it very hard for the mutator to vary them independently.
    let (bookmark_src, log_src) = match text.split_once("\n@@\n") {
        Some((a, b)) => (a, b),
        None => (text.as_ref(), text.as_ref()),
    };

    let Ok((bookmarks, _)) = parse_bookmark_output(bookmark_src) else {
        return;
    };
    let Ok(changes) = parse_log_output(log_src) else {
        return;
    };
    // Keep the input small enough that a timeout means a LOOP rather than a big graph.
    if bookmarks.len() > 64 || changes.len() > 256 {
        return;
    }

    let jj = FuzzedJj { bookmarks, changes };
    let Ok(graph) = build_change_graph(&jj) else {
        return; // a rejected graph is a fine outcome; an incoherent one is not
    };

    // Every child→parent chain must reach an end without revisiting a node.
    for start in graph.adjacency_list.keys() {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut node: &str = start;
        while let Some(parent) = graph.adjacency_list.get(node) {
            assert!(
                seen.insert(node),
                "adjacency_list contains a cycle reachable from {start:?} \
                 (revisited {node:?}) — traversal toward trunk would not terminate"
            );
            node = parent;
        }
    }

    // A bookmark's change id must be one the graph actually knows about, or code
    // that looks a bookmark up and then indexes the segment map gets None where it
    // structurally expects Some.
    for (name, change_id) in &graph.bookmark_to_change_id {
        assert!(
            graph.bookmarks.contains_key(name),
            "bookmark_to_change_id names {name:?}, which is absent from bookmarks"
        );
        assert!(
            !change_id.is_empty(),
            "bookmark {name:?} mapped to an empty change id"
        );
    }
});
