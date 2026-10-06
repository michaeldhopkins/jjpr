//! Restack a stack whose bottom was merged out of band, before submit pushes it.
//!
//! When the bottom PR is squash-merged on the forge, its commits never enter
//! trunk: trunk gets one new commit with the same content. The survivor above
//! still sits on the old commits, so pushing it as-is opens (or updates) a PR
//! whose diff re-includes the merged work. `merge` and `watch` rebase the
//! survivor after merging; this gives `submit` the same step (issue #10).
//!
//! What says a commit below the survivor merged, strongest first:
//!
//! - The merged bookmark still exists (the forge kept the branch), and the
//!   plan's merged check found it.
//! - The forge reports a PR merged into trunk whose head is that very commit.
//!   This needs no local history, so it works however the bookmark was lost:
//!   submit's own fetch, an earlier `jj git fetch`, or a fresh clone. It is
//!   asked whenever no deleted bookmark already names its exact commit.
//! - Submit's fetch deleted a bookmark whose PR merged from a different commit
//!   (the forge rewrote it before merging). Which commits landed is unknown, so
//!   everything from trunk up is rebased with `--skip-emptied`, dropping what
//!   the rebase empties.
//!
//! With a known merged commit, only the commits above it move onto trunk, so
//! the merged content cannot conflict, and the merged commits left behind are
//! abandoned unless something else of yours still builds on them.

use std::collections::HashSet;

use anyhow::{Context, Result};

use super::plan::SubmissionPlan;
pub use super::restack_messages::{
    abandon_failed_warning, lookup_failed_warning, merge_commit_warning, restack_note,
};
use crate::forge::Forge;
use crate::forge::merged::{branch_name, merged_from};
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
    /// The `-d` destinations: trunk, plus a merge commit's other parents.
    pub onto: Vec<String>,
    /// Whether the rebase drops commits it empties: the fallback when it is
    /// not known which commits merged.
    pub skip_emptied: bool,
    /// The merged head commit whose unbookmarked commits, from trunk up, are
    /// abandoned once the survivor no longer sits on them.
    pub abandon: Option<String>,
}

/// What [`plan_restack`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing below the survivor merged; leave the stack alone.
    Leave,
    Rebase(Restack),
    /// Something below merged, but the commits between it and the survivor are
    /// not a single line, so which to move is unclear.
    CannotTell,
}

/// Commits below the first live segment's bookmark that merged on the forge.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MergedBelow {
    /// Head commits of PRs merged into trunk: they and everything under them
    /// down to trunk landed.
    pub heads: HashSet<String>,
    /// Commits whose deleted bookmark's PR merged from a different commit.
    pub rewritten: HashSet<String>,
    /// The merged branches, for the message.
    pub names: Vec<String>,
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

/// Decide the restack. `merged` names segments the plan found merged with
/// their bookmark still present; `ancestry` is the first live segment's
/// `trunk()..` range, newest first. Only the first live segment is rebased:
/// `jj rebase -s` carries everything above it along.
pub fn plan_restack(
    segments: &[NarrowedSegment],
    merged: &HashSet<&str>,
    below: &MergedBelow,
    ancestry: &[LogEntry],
) -> Decision {
    let Some((idx, live)) = first_live(segments, merged) else {
        return Decision::Leave;
    };
    let kept: HashSet<&str> = segments[..idx]
        .iter()
        .map(|s| s.bookmark.commit_id.as_str())
        .collect();
    // ancestry[0] is the survivor's own tip; a merged base sits below it.
    let is_head =
        |c: &LogEntry| kept.contains(c.commit_id.as_str()) || below.heads.contains(&c.commit_id);
    let head = ancestry.iter().skip(1).position(is_head).map(|i| i + 1);
    let rewritten = ancestry
        .iter()
        .skip(1)
        .position(|c| below.rewritten.contains(&c.commit_id))
        .map(|i| i + 1);
    let restack = |root: String, onto: Vec<String>, skip_emptied: bool, abandon: Option<String>| {
        Decision::Rebase(Restack {
            bookmark: live.bookmark.name.clone(),
            root,
            onto,
            skip_emptied,
            abandon,
        })
    };
    let trunk = || vec![TRUNK.to_string()];
    match (head, rewritten) {
        (_, Some(r)) if head.is_none_or(|h| r < h) => match ancestry.last() {
            Some(oldest) => restack(oldest.change_id.clone(), trunk(), true, None),
            None => Decision::Leave,
        },
        (Some(h), _) => {
            let id = &ancestry[h].commit_id;
            let Some((child, onto)) = swap_merged_parent(ancestry, h) else {
                return Decision::CannotTell;
            };
            let abandon = (!kept.contains(id.as_str())).then(|| id.clone());
            restack(child.change_id.clone(), onto, false, abandon)
        }
        _ if idx > 0 => restack(rebase_root(live).to_string(), trunk(), false, None),
        _ => Decision::Leave,
    }
}

/// The revset every restack moves the survivor onto.
const TRUNK: &str = "trunk()";

/// The one commit above the merged head `ancestry[h]`, and its parents with
/// that head swapped for trunk. A merge commit keeps its other parents, which
/// is safe only while none of them is itself merged work (the head or below
/// it): that would carry what merged, so it gives `None`, as do several
/// commits building on the head.
fn swap_merged_parent(ancestry: &[LogEntry], h: usize) -> Option<(&LogEntry, Vec<String>)> {
    let id = &ancestry[h].commit_id;
    let mut children = ancestry[..h].iter().filter(|c| c.parents.contains(id));
    let child = children.next()?;
    if children.next().is_some() {
        return None;
    }
    let landed = ancestors_within(ancestry, id);
    let mut onto = Vec::new();
    for parent in &child.parents {
        if parent == id {
            onto.push(TRUNK.to_string());
        } else if landed.contains(parent.as_str()) {
            return None;
        } else {
            onto.push(parent.clone());
        }
    }
    Some((child, onto))
}

/// `head` and its ancestors among `ancestry`, following recorded parents.
fn ancestors_within<'a>(ancestry: &'a [LogEntry], head: &'a str) -> HashSet<&'a str> {
    let mut seen = HashSet::from([head]);
    let mut queue = vec![head];
    while let Some(id) = queue.pop() {
        let parents = ancestry.iter().filter(|c| c.commit_id == id);
        for parent in parents.flat_map(|c| &c.parents) {
            if seen.insert(parent.as_str()) {
                queue.push(parent.as_str());
            }
        }
    }
    seen
}

/// The commits to abandon below a merged `head` once the survivor has moved
/// off it: everything from trunk up to `head`, except what a bookmark, the
/// working copy or another line of work still stands on.
pub fn abandon_revset(head: &str) -> String {
    let landed = format!(r#"(trunk().."{head}")"#);
    format!("{landed} ~ ::((visible_heads() ~ {landed}) | bookmarks() | @)")
}

/// Find what merged below the first live segment: bookmarks submit's fetch
/// deleted whose PR merged, then, unless one of those named its exact commit,
/// recently merged PRs whose head is a commit below the segment's tip.
fn find_merged_below(
    jj: &dyn Jj,
    forge: &dyn Forge,
    plan: &SubmissionPlan,
    live: &NarrowedSegment,
    ancestry: &[LogEntry],
    before_fetch: &[Bookmark],
) -> Result<MergedBelow> {
    let (owner, repo) = (&plan.repo_info.owner, &plan.repo_info.repo);
    let mut found = MergedBelow::default();
    if !before_fetch.is_empty() {
        let after: HashSet<String> = jj.get_my_bookmarks()?.into_iter().map(|b| b.name).collect();
        for gone in vanished_in_stack(before_fetch, &after, ancestry) {
            match forge.find_merged_pr(owner, repo, &gone.name) {
                Ok(Some(pr)) => {
                    let exact =
                        !gone.commit_id.is_empty() && pr.head.sha.starts_with(&gone.commit_id);
                    let into = if exact {
                        &mut found.heads
                    } else {
                        &mut found.rewritten
                    };
                    into.insert(gone.commit_id);
                    found.names.push(gone.name);
                }
                Ok(None) => {}
                Err(e) => eprintln!(
                    "  Warning: could not check merged status for '{}': {e}",
                    gone.name
                ),
            }
        }
    }
    if !found.heads.is_empty() || ancestry.len() < 2 {
        return Ok(found);
    }
    match forge.list_recently_merged_prs(owner, repo) {
        Ok(prs) => {
            for commit in &ancestry[1..] {
                if let Some(pr) = merged_from(&prs, &commit.commit_id, &plan.default_branch) {
                    found.heads.insert(commit.commit_id.clone());
                    found.names.push(branch_name(pr, plan.forge_kind));
                }
            }
        }
        Err(e) => {
            let oldest = ancestry.last().map_or("", |c| c.change_id.as_str());
            let name = &live.bookmark.name;
            let warning = lookup_failed_warning(name, &plan.default_branch, oldest, &e.to_string());
            eprintln!("{warning}");
        }
    }
    Ok(found)
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
    if !plan.dry_run {
        let (owner, repo) = (&plan.repo_info.owner, &plan.repo_info.repo);
        super::stale::forget_merged(jj, forge, owner, repo, plan.forge_kind);
    }
    let merged: HashSet<&str> = plan
        .bookmarks_already_merged
        .iter()
        .map(|m| m.bookmark.name.as_str())
        .collect();
    let Some((idx, live)) = first_live(segments, &merged).filter(|_| !foreign_base) else {
        return Ok(false);
    };
    let ancestry = jj.get_changes_to_commit(&live.bookmark.commit_id)?;
    let mut below = find_merged_below(jj, forge, plan, live, &ancestry, before_fetch)?;
    let mut names: Vec<String> = segments[..idx]
        .iter()
        .map(|s| s.bookmark.name.clone())
        .collect();
    names.append(&mut below.names);
    names.sort();
    names.dedup();
    let trunk = &plan.default_branch;
    // A merged segment still listed is one whose commits trunk lacks (a squash
    // or rebase landing), so the survivor on top of it is never already based
    // on trunk. A merge-commit landing puts the commits in trunk, the segment
    // drops out of `trunk()..`, and nothing is planned.
    let restack = match plan_restack(segments, &merged, &below, &ancestry) {
        Decision::Leave => return Ok(false),
        Decision::CannotTell => {
            eprintln!(
                "{}",
                merge_commit_warning(&live.bookmark.name, trunk, &names)
            );
            return Ok(false);
        }
        Decision::Rebase(restack) => restack,
    };
    let note = restack_note(&restack.bookmark, trunk, names);
    if plan.dry_run {
        println!("Would rebase {note}\n");
        return Ok(false);
    }
    println!("Rebasing {note}...\n");
    let rebase = if restack.skip_emptied {
        jj.rebase_onto_skipping_emptied(&restack.root, TRUNK)
    } else if restack.onto == [TRUNK] {
        jj.rebase_onto(&restack.root, TRUNK)
    } else {
        jj.rebase_onto_all(&restack.root, &restack.onto)
    };
    let bookmark = &restack.bookmark;
    rebase.with_context(|| super::restack_messages::rebase_failed(&restack, trunk))?;
    if let Some(head) = &restack.abandon {
        // The survivor is already safe on trunk; a failure here only leaves
        // the merged commits visible, so it warns rather than stopping submit.
        if let Err(e) = jj.abandon(&abandon_revset(head)) {
            eprintln!("{}", abandon_failed_warning(bookmark, head, &e.to_string()));
        }
    }
    Ok(true)
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
    eprintln!(
        "To resolve one: jj new <change_id>, fix the files, then jj squash. Then run jjpr submit again."
    );
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

    /// A line of commits, newest first, each the parent of the one before.
    fn changes(commits: &[&str]) -> Vec<LogEntry> {
        let mut line: Vec<LogEntry> = commits.iter().map(|c| change(c)).collect();
        for i in 0..line.len().saturating_sub(1) {
            line[i].parents = vec![commits[i + 1].to_string()];
        }
        line
    }

    fn below(heads: &[&str], rewritten: &[&str]) -> MergedBelow {
        MergedBelow {
            heads: heads.iter().map(|c| c.to_string()).collect(),
            rewritten: rewritten.iter().map(|c| c.to_string()).collect(),
            names: vec![],
        }
    }

    fn rebase(root: &str, skip_emptied: bool, abandon: Option<&str>) -> Decision {
        rebase_onto(root, &["trunk()"], skip_emptied, abandon)
    }

    fn rebase_onto(
        root: &str,
        onto: &[&str],
        skip_emptied: bool,
        abandon: Option<&str>,
    ) -> Decision {
        Decision::Rebase(Restack {
            bookmark: "top".to_string(),
            root: root.to_string(),
            onto: onto.iter().map(|d| d.to_string()).collect(),
            skip_emptied,
            abandon: abandon.map(str::to_string),
        })
    }

    /// Issue #10 with the bookmark gone: the forge says `b2` was a merged PR's
    /// head, so only what sits above it moves, and `b2` down to trunk is
    /// abandoned. Nothing depends on the merged content applying cleanly.
    #[test]
    fn a_merged_head_below_moves_only_what_sits_above_it() {
        let segments = [segment("top", &["t2"])];
        let ancestry = changes(&["t2", "t1", "b2", "b1"]);
        assert_eq!(
            plan_restack(&segments, &set(&[]), &below(&["b2"], &[]), &ancestry),
            rebase("ch_t1", false, Some("b2"))
        );
    }

    /// Submit's fetch deleted `bottom`, whose PR merged from another commit:
    /// which commits landed is unknown, so everything from trunk up is rebased,
    /// dropping what the rebase empties.
    #[test]
    fn a_rewritten_merge_below_restacks_from_trunk_skipping_emptied() {
        let segments = [segment("top", &["t1"])];
        let ancestry = changes(&["t1", "b2", "b1"]);
        assert_eq!(
            plan_restack(&segments, &set(&[]), &below(&[], &["b2"]), &ancestry),
            rebase("ch_b1", true, None)
        );
    }

    /// Issue #10 with the branch kept: `bottom` is still a segment, found
    /// merged by the plan, and `top` is rebased from its own oldest commit.
    /// Its bookmark keeps its commits, so nothing is abandoned.
    #[test]
    fn a_merged_segment_below_restacks_the_first_live_one() {
        let segments = [
            segment("bottom", &["b1"]),
            segment("top", &["t2", "t1"]),
            segment("leaf", &["l1"]),
        ];
        let merged = set(&["bottom"]);
        let expected = rebase("ch_t1", false, None);
        assert_eq!(
            plan_restack(&segments, &merged, &below(&[], &[]), &[]),
            expected
        );
        let ancestry = changes(&["t2", "t1", "b1"]);
        assert_eq!(
            plan_restack(&segments, &merged, &below(&[], &[]), &ancestry),
            expected
        );
    }

    /// The survivor starts with a merge of the merged head and other work.
    /// Swapping only the merged parent for trunk keeps the other parent, so
    /// jjpr does it itself.
    #[test]
    fn a_merge_commit_above_the_merged_head_keeps_its_other_parent() {
        let segments = [segment("top", &["t2"])];
        let mut ancestry = changes(&["t2", "t1", "o1", "b1"]);
        ancestry[1].parents = vec!["b1".to_string(), "o1".to_string()];
        ancestry[2].parents = vec![];
        assert_eq!(
            plan_restack(&segments, &set(&[]), &below(&["b1"], &[]), &ancestry),
            rebase_onto("ch_t1", &["trunk()", "o1"], false, Some("b1"))
        );
    }

    /// The merge's other parent is itself merged work (below the merged head):
    /// keeping it would carry what merged, and dropping it would change the
    /// merge. jjpr does not guess.
    #[test]
    fn a_merge_whose_other_parent_also_merged_cannot_be_told_apart() {
        let segments = [segment("top", &["t1"])];
        let mut ancestry = changes(&["t1", "b2", "b1"]);
        ancestry[0].parents = vec!["b2".to_string(), "b1".to_string()];
        assert_eq!(
            plan_restack(&segments, &set(&[]), &below(&["b2"], &[]), &ancestry),
            Decision::CannotTell
        );
    }

    /// Two commits of the survivor's both build on the merged head: no single
    /// rebase root, so jjpr does not guess.
    #[test]
    fn two_children_of_the_merged_head_cannot_be_told_apart() {
        let segments = [segment("top", &["t3"])];
        let mut ancestry = changes(&["t3", "t2", "t1", "b1"]);
        ancestry[0].parents = vec!["t2".to_string(), "t1".to_string()];
        ancestry[1].parents = vec!["b1".to_string()];
        assert_eq!(
            plan_restack(&segments, &set(&[]), &below(&["b1"], &[]), &ancestry),
            Decision::CannotTell
        );
    }

    #[test]
    fn nothing_merged_or_everything_merged_needs_no_restack() {
        let segments = [segment("bottom", &["b1"]), segment("top", &["t1"])];
        let ancestry = changes(&["b1"]);
        let none = below(&[], &[]);
        assert_eq!(
            plan_restack(&segments, &set(&[]), &none, &ancestry),
            Decision::Leave
        );
        let all = set(&["bottom", "top"]);
        let found = below(&["b1"], &["b1"]);
        assert_eq!(
            plan_restack(&segments, &all, &found, &ancestry),
            Decision::Leave
        );
        assert_eq!(
            plan_restack(&[], &set(&[]), &found, &ancestry),
            Decision::Leave
        );
        // The survivor's own tip reported merged says nothing about below it.
        assert_eq!(
            plan_restack(&segments, &set(&[]), &found, &ancestry),
            Decision::Leave
        );
    }

    #[test]
    fn abandon_revset_spares_what_anything_else_stands_on() {
        assert_eq!(
            abandon_revset("b2"),
            r#"(trunk().."b2") ~ ::((visible_heads() ~ (trunk().."b2")) | bookmarks() | @)"#
        );
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
    /// choice of merged heads and rewritten merges among the live segment's
    /// ancestry: the restack always names the first unmerged segment; it is
    /// planned exactly when something below the tip merged or a segment below
    /// was merged; it skips emptied commits exactly when a rewritten merge sits
    /// above every known head; otherwise its root is the child of the merged
    /// head it abandons, and the root is never itself a commit known to have
    /// merged.
    #[test]
    fn plan_restack_invariants_hold_for_every_small_stack() {
        let segments = [
            segment("a", &["a2", "a1"]),
            segment("b", &["b1"]),
            segment("c", &["c2", "c1"]),
        ];
        let names = ["a", "b", "c"];
        let ancestry = changes(&["c2", "c1", "x2", "x1"]);
        let pick = |mask: u32| -> Vec<&str> {
            (0..4)
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| ancestry[i].commit_id.as_str())
                .collect()
        };
        for merged_mask in 0..8u32 {
            for heads_mask in 0..16u32 {
                for rewritten_mask in 0..16u32 {
                    let merged: HashSet<&str> = (0..3)
                        .filter(|i| merged_mask & (1 << i) != 0)
                        .map(|i| names[i])
                        .collect();
                    let found = below(&pick(heads_mask), &pick(rewritten_mask));
                    let got = plan_restack(&segments, &merged, &found, &ancestry);
                    let ctx = format!("{merged:?} {found:?}");
                    let Some(idx) = segments
                        .iter()
                        .position(|s| !merged.contains(s.bookmark.name.as_str()))
                    else {
                        assert_eq!(got, Decision::Leave, "{ctx}");
                        continue;
                    };
                    let first_below = |mask: u32| (1..4).find(|i| mask & (1 << i) != 0);
                    let (head, rewritten) = (first_below(heads_mask), first_below(rewritten_mask));
                    if idx == 0 && head.is_none() && rewritten.is_none() {
                        assert_eq!(got, Decision::Leave, "{ctx}");
                        continue;
                    }
                    let Decision::Rebase(restack) = got else {
                        panic!("expected a rebase: {ctx} {got:?}");
                    };
                    assert_eq!(restack.bookmark, segments[idx].bookmark.name, "{ctx}");
                    let skip = rewritten.is_some_and(|r| head.is_none_or(|h| r < h));
                    assert_eq!(restack.skip_emptied, skip, "{ctx}");
                    if skip {
                        assert_eq!(restack.root, "ch_x1", "{ctx}");
                        assert_eq!(restack.abandon, None, "{ctx}");
                        continue;
                    }
                    let root = ancestry.iter().find(|c| c.change_id == restack.root);
                    match (&restack.abandon, root) {
                        (Some(gone), Some(root)) => {
                            assert_eq!(root.parents, vec![gone.clone()], "{ctx}");
                            assert_eq!(restack.onto, vec!["trunk()".to_string()], "{ctx}");
                            assert!(found.heads.contains(gone), "{ctx}");
                            assert!(
                                !found.heads.contains(&root.commit_id) || root.commit_id == "c2"
                            );
                        }
                        (None, _) => {
                            assert!(head.is_none() && idx > 0, "{ctx}");
                            assert_eq!(restack.root, rebase_root(&segments[idx]), "{ctx}");
                        }
                        (Some(_), None) => panic!("root outside the ancestry: {ctx}"),
                    }
                }
            }
        }
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
