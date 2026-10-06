//! Restack a stack whose bottom was merged out of band, before submit pushes it.
//!
//! When the bottom PR is squash-merged on the forge, its commits never enter
//! trunk: trunk gets one new commit with the same content. The survivor above
//! still sits on the old commits, so pushing it as-is opens (or updates) a PR
//! whose diff re-includes the merged work. `merge` and `watch` rebase the
//! survivor after merging; this gives `submit` the same step (issue #10).
//!
//! Two shapes reach submit:
//!
//! - The merged bookmark still exists (the forge kept the branch). The plan's
//!   merged check finds it, and the first live segment is rebased onto trunk.
//! - The fetch deleted the merged bookmark (the forge deleted the branch). Its
//!   commits stay below the survivor, now unbookmarked, and would be pushed
//!   with it. jjpr notes which of your bookmarks the fetch removed, asks the
//!   forge whether each merged, and rebases everything between trunk and the
//!   survivor with `--skip-emptied`: the merged commits become empty on top of
//!   trunk and are dropped. A bookmark deleted by a fetch outside jjpr leaves
//!   nothing to compare against, so that case is not detected.

use std::collections::HashSet;

use anyhow::{Context, Result};

use super::plan::SubmissionPlan;
use crate::forge::Forge;
use crate::jj::Jj;
use crate::jj::types::{Bookmark, LogEntry, NarrowedSegment};
use crate::merge::execute::rebase_root;

/// The one rebase that restacks the survivor onto trunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restack {
    /// The first live segment's bookmark.
    pub bookmark: String,
    /// Change id handed to `jj rebase -s`.
    pub root: String,
    /// Whether the rebase drops commits it empties: merged commits whose
    /// bookmark the fetch deleted sit below the survivor.
    pub skip_emptied: bool,
}

/// The first segment the plan did not find merged, with its index.
pub fn first_live<'a>(
    segments: &'a [NarrowedSegment],
    merged: &HashSet<&str>,
) -> Option<(usize, &'a NarrowedSegment)> {
    segments
        .iter()
        .enumerate()
        .find(|(_, s)| !merged.contains(s.bookmark.name.as_str()))
}

/// Bookmarks present before the fetch and gone after it whose commit is in
/// `ancestry` (the survivor's `trunk()..` range): the ones it would carry.
pub fn vanished_in_stack(
    before: &[Bookmark],
    after: &HashSet<String>,
    ancestry: &[LogEntry],
) -> Vec<Bookmark> {
    let carried: HashSet<&str> = ancestry.iter().map(|c| c.commit_id.as_str()).collect();
    before
        .iter()
        .filter(|b| !after.contains(&b.name) && carried.contains(b.commit_id.as_str()))
        .cloned()
        .collect()
}

/// Decide the restack, if any. `merged` names segments the plan found merged
/// with their bookmark still present; `vanished_merged` holds the commit ids of
/// merged bookmarks the fetch deleted; `ancestry` is the first live segment's
/// `trunk()..` range, newest first. Only the first live segment is rebased:
/// `jj rebase -s` carries everything above it along.
pub fn plan_restack(
    segments: &[NarrowedSegment],
    merged: &HashSet<&str>,
    vanished_merged: &HashSet<&str>,
    ancestry: &[LogEntry],
) -> Option<Restack> {
    let (idx, live) = first_live(segments, merged)?;
    let skip_emptied = ancestry
        .iter()
        .any(|c| vanished_merged.contains(c.commit_id.as_str()));
    let root = match ancestry.last() {
        Some(oldest) if skip_emptied => oldest.change_id.clone(),
        _ => rebase_root(live).to_string(),
    };
    (skip_emptied || idx > 0).then(|| Restack {
        bookmark: live.bookmark.name.clone(),
        root,
        skip_emptied,
    })
}

/// Rebase the survivor of an out-of-band merge onto trunk. Returns whether the
/// stack was rewritten, in which case the caller must rebuild its segments and
/// plan. A dry run only reports what it would do. Skipped under a foreign or
/// overridden base, where "onto trunk" would be wrong.
pub fn restack_merged_base(
    jj: &dyn Jj,
    forge: &dyn Forge,
    plan: &SubmissionPlan,
    segments: &[NarrowedSegment],
    before_fetch: &[Bookmark],
    foreign_base: bool,
) -> Result<bool> {
    let merged: HashSet<&str> = plan
        .bookmarks_already_merged
        .iter()
        .map(|m| m.bookmark.name.as_str())
        .collect();
    let Some((idx, live)) = first_live(segments, &merged).filter(|_| !foreign_base) else {
        return Ok(false);
    };
    let mut merged_names: Vec<String> = segments[..idx]
        .iter()
        .map(|s| s.bookmark.name.clone())
        .collect();
    let mut vanished_merged = HashSet::new();
    let mut ancestry = Vec::new();
    if !before_fetch.is_empty() {
        let after: HashSet<String> = jj.get_my_bookmarks()?.into_iter().map(|b| b.name).collect();
        ancestry = jj.get_changes_to_commit(&live.bookmark.commit_id)?;
        let (owner, repo) = (&plan.repo_info.owner, &plan.repo_info.repo);
        for gone in vanished_in_stack(before_fetch, &after, &ancestry) {
            match forge.find_merged_pr(owner, repo, &gone.name) {
                Ok(Some(_)) => {
                    merged_names.push(gone.name);
                    vanished_merged.insert(gone.commit_id);
                }
                Ok(None) => {}
                Err(e) => eprintln!(
                    "  Warning: could not check merged status for '{}': {e}",
                    gone.name
                ),
            }
        }
    }
    let vanished: HashSet<&str> = vanished_merged.iter().map(String::as_str).collect();
    let Some(restack) = plan_restack(segments, &merged, &vanished, &ancestry) else {
        return Ok(false);
    };
    // No is_rooted_in check as in merge's reconcile: segments are built after
    // the fetch from `trunk()..`, so a merged segment still listed is one whose
    // commits trunk lacks (a squash or rebase landing), and the survivor on top
    // of it is never already based on trunk. A merge-commit landing puts the
    // commits in trunk, the segment drops out, and no restack is planned.
    let note = restack_note(&restack.bookmark, &plan.default_branch, merged_names);
    if plan.dry_run {
        println!("Would rebase {note}\n");
        return Ok(false);
    }
    println!("Rebasing {note}...\n");
    let rebase = if restack.skip_emptied {
        jj.rebase_onto_skipping_emptied(&restack.root, "trunk()")
    } else {
        jj.rebase_onto(&restack.root, "trunk()")
    };
    let (bookmark, trunk) = (&restack.bookmark, &plan.default_branch);
    rebase.with_context(|| format!("failed to rebase '{bookmark}' onto {trunk}"))?;
    Ok(true)
}

/// `'top' onto main ('bottom' below it was merged)`: what the restack does and
/// why, after "Rebasing" or "Would rebase".
pub fn restack_note(bookmark: &str, trunk: &str, mut merged: Vec<String>) -> String {
    merged.sort();
    let verb = if merged.len() == 1 { "was" } else { "were" };
    let names = merged.join("', '");
    format!("'{bookmark}' onto {trunk} ('{names}' below it {verb} merged)")
}

/// The target's segments rebuilt after a restack, keeping the bookmark chosen
/// for each segment before, and screened for conflicts the rebase introduced.
pub fn rebuild_segments(
    jj: &dyn Jj,
    target: &str,
    previous: &[NarrowedSegment],
) -> Result<Vec<NarrowedSegment>> {
    let graph = crate::graph::change_graph::build_change_graph(jj)?;
    let analysis = super::analyze::analyze_submission_graph(&graph, target)?;
    let segments = super::resolve::narrow_like(&analysis.relevant_segments, previous)?;
    refuse_conflicts(&segments)?;
    Ok(segments)
}

/// Refuse a stack holding conflicted commits before anything is pushed, naming
/// each one. jj will not push a conflict, so this is the earlier, clearer stop.
pub fn refuse_conflicts(segments: &[NarrowedSegment]) -> Result<()> {
    let conflicted: Vec<_> = segments
        .iter()
        .flat_map(|seg| {
            seg.changes.iter().filter(|c| c.conflict).map(|c| {
                (
                    seg.bookmark.name.as_str(),
                    c.change_id.as_str(),
                    c.description_first_line.as_str(),
                )
            })
        })
        .collect();
    if conflicted.is_empty() {
        return Ok(());
    }
    eprintln!("Error: cannot push; some commits have unresolved conflicts:\n");
    for (bookmark, change_id, desc) in &conflicted {
        eprintln!("  {change_id} ({bookmark}): {desc}");
    }
    eprintln!();
    eprintln!("To resolve: jj edit <change_id>, fix the conflicts, then re-run jjpr submit.");
    anyhow::bail!("unresolved conflicts in stack");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::types::LogEntry;

    fn change(commit: &str) -> LogEntry {
        LogEntry {
            commit_id: commit.to_string(),
            change_id: format!("ch_{commit}"),
            author_name: String::new(),
            author_email: String::new(),
            description: String::new(),
            description_first_line: String::new(),
            parents: vec![],
            local_bookmarks: vec![],
            remote_bookmarks: vec![],
            is_working_copy: false,
            conflict: false,
            empty: false,
        }
    }

    fn bookmark(name: &str, commit: &str) -> Bookmark {
        Bookmark {
            name: name.to_string(),
            commit_id: commit.to_string(),
            change_id: format!("ch_{commit}"),
            has_remote: true,
            is_synced: true,
        }
    }

    /// A segment named `name` holding `commits`, newest first as jj lists them.
    fn segment(name: &str, commits: &[&str]) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: bookmark(name, commits[0]),
            changes: commits.iter().map(|c| change(c)).collect(),
            merge_source_names: vec![],
        }
    }

    fn set<'a>(items: &[&'a str]) -> HashSet<&'a str> {
        items.iter().copied().collect()
    }

    fn changes(commits: &[&str]) -> Vec<LogEntry> {
        commits.iter().map(|c| change(c)).collect()
    }

    /// Issue #10, after the fetch deleted `bottom`: its commit sits below
    /// `top`, unbookmarked, so everything from trunk up is rebased, dropping
    /// what the rebase empties.
    #[test]
    fn a_deleted_merged_bookmark_below_restacks_from_trunk_skipping_emptied() {
        let segments = [segment("top", &["t1"])];
        let ancestry = changes(&["t1", "b2", "b1"]);
        let restack = plan_restack(&segments, &set(&[]), &set(&["b2"]), &ancestry).unwrap();
        assert_eq!(
            restack,
            Restack {
                bookmark: "top".to_string(),
                root: "ch_b1".to_string(),
                skip_emptied: true,
            }
        );
    }

    /// Issue #10 with the branch kept: `bottom` is still a segment, found
    /// merged by the plan, and `top` is rebased from its own oldest commit.
    #[test]
    fn a_merged_segment_below_restacks_the_first_live_one() {
        let segments = [
            segment("bottom", &["b1"]),
            segment("top", &["t2", "t1"]),
            segment("leaf", &["l1"]),
        ];
        let restack = plan_restack(&segments, &set(&["bottom"]), &set(&[]), &[]).unwrap();
        assert_eq!(restack.bookmark, "top");
        assert_eq!(restack.root, "ch_t1");
        assert!(!restack.skip_emptied);
    }

    #[test]
    fn nothing_merged_or_everything_merged_needs_no_restack() {
        let segments = [segment("bottom", &["b1"]), segment("top", &["t1"])];
        let ancestry = changes(&["b1"]);
        assert_eq!(
            plan_restack(&segments, &set(&[]), &set(&[]), &ancestry),
            None
        );
        let all = set(&["bottom", "top"]);
        assert_eq!(
            plan_restack(&segments, &all, &set(&["b1"]), &ancestry),
            None
        );
        assert_eq!(plan_restack(&[], &set(&[]), &set(&["b1"]), &ancestry), None);
    }

    #[test]
    fn first_live_skips_merged_segments() {
        let segments = [segment("a", &["a1"]), segment("b", &["b1"])];
        assert_eq!(first_live(&segments, &set(&[])).map(|(i, _)| i), Some(0));
        assert_eq!(first_live(&segments, &set(&["a"])).map(|(i, _)| i), Some(1));
        assert!(first_live(&segments, &set(&["a", "b"])).is_none());
    }

    #[test]
    fn vanished_in_stack_keeps_only_deleted_bookmarks_the_survivor_carries() {
        let ancestry = changes(&["t1", "b1"]);
        let before = [
            bookmark("bottom", "b1"),    // deleted, below the survivor: vanished
            bookmark("top", "t1"),       // still there
            bookmark("elsewhere", "z9"), // deleted, but not carried
        ];
        let after: HashSet<String> = ["top".to_string()].into_iter().collect();
        let gone = vanished_in_stack(&before, &after, &ancestry);
        assert_eq!(gone, vec![bookmark("bottom", "b1")]);
    }

    /// Property over every way to mark a three-segment stack merged and every
    /// commit of the first live segment's ancestry vanished: a restack, when
    /// planned, names the first unmerged segment, skips emptied commits exactly
    /// when the ancestry holds a vanished one and then roots at its oldest
    /// commit, else at the segment's own oldest; with nothing merged below and
    /// nothing vanished, there is none.
    #[test]
    fn plan_restack_invariants_hold_for_every_small_stack() {
        let segments = [
            segment("a", &["a2", "a1"]),
            segment("b", &["b1"]),
            segment("c", &["c2", "c1"]),
        ];
        let names = ["a", "b", "c"];
        let ancestry = changes(&["c2", "c1", "x2", "x1"]);
        for merged_mask in 0..8u32 {
            for vanished_mask in 0..16u32 {
                let merged: HashSet<&str> = (0..3)
                    .filter(|i| merged_mask & (1 << i) != 0)
                    .map(|i| names[i])
                    .collect();
                let vanished: HashSet<&str> = (0..4)
                    .filter(|i| vanished_mask & (1 << i) != 0)
                    .map(|i| ancestry[i].commit_id.as_str())
                    .collect();
                let got = plan_restack(&segments, &merged, &vanished, &ancestry);
                let Some(idx) = segments
                    .iter()
                    .position(|s| !merged.contains(s.bookmark.name.as_str()))
                else {
                    assert_eq!(got, None, "{merged:?}");
                    continue;
                };
                let live = &segments[idx];
                let holds_vanished = !vanished.is_empty();
                if idx == 0 && !holds_vanished {
                    assert_eq!(got, None, "{merged:?} {vanished:?}");
                    continue;
                }
                let restack = got.unwrap_or_else(|| panic!("{merged:?} {vanished:?}"));
                assert_eq!(restack.bookmark, live.bookmark.name);
                assert_eq!(restack.skip_emptied, holds_vanished);
                let root = if holds_vanished {
                    "ch_x1"
                } else {
                    rebase_root(live)
                };
                assert_eq!(restack.root, root);
            }
        }
    }

    #[test]
    fn restack_note_names_what_merged_in_order_and_agrees_in_number() {
        assert_eq!(
            restack_note("top", "main", vec!["bottom".to_string()]),
            "'top' onto main ('bottom' below it was merged)"
        );
        assert_eq!(
            restack_note("top", "main", vec!["b".to_string(), "a".to_string()]),
            "'top' onto main ('a', 'b' below it were merged)"
        );
    }

    #[test]
    fn refuse_conflicts_names_a_conflicted_commit_and_passes_a_clean_stack() {
        let clean = [segment("top", &["t1"])];
        assert!(refuse_conflicts(&clean).is_ok());
        let mut conflicted = segment("top", &["t2", "t1"]);
        conflicted.changes[1].conflict = true;
        let err = refuse_conflicts(&[conflicted]).unwrap_err();
        assert!(err.to_string().contains("unresolved conflicts"), "{err}");
    }
}
