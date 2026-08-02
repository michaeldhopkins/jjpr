//! Proven scenarios for jjpr's working-copy handling, run against a real `jj`.
//! jjpr is a working-copy-agnostic background actor; these lock that in:
//!
//! - reads/queries (what a watch poll runs) never snapshot the user's WIP;
//! - the reconcile rebase AND merge leave an unrelated `@` untouched (no WIP
//!   snapshot, no moved `@`), yet correctly update the working copy when `@` IS
//!   in the subtree being rebased — no stale-working-copy error, edits carried
//!   along (ignoring the working copy there would strand the user);
//! - the explicit snapshot the submit path uses DOES fold WIP into `@`.
//!
//! Gated on `jj` installed.

use std::path::Path;
use std::process::{Command, Output};

use jjpr::jj::{Jj, JjRunner};
use tempfile::TempDir;

mod common;
use common::jj_available;

struct Repo {
    dir: TempDir,
}
impl Repo {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        assert!(
            Command::new("jj").args(["git", "init"]).current_dir(dir.path()).output().unwrap().status.success(),
            "jj git init failed"
        );
        // Set the user at the repo level so JjRunner (which doesn't pass a
        // per-command --config) sees the same author as our commits — otherwise
        // `mine()` in get_my_bookmarks matches nothing.
        for (k, v) in [("user.name", "T"), ("user.email", "t@e.com")] {
            Command::new("jj").args(["config", "set", "--repo", k, v]).current_dir(dir.path()).output().unwrap();
        }
        Self { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn jj(&self, args: &[&str]) -> Output {
        Command::new("jj")
            .args(["--config=user.name=T", "--config=user.email=t@e.com"])
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("jj command")
    }
    fn out(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.jj(args).stdout).trim().to_string()
    }
    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path().join(name), content).unwrap();
    }
    fn runner(&self) -> JjRunner {
        JjRunner::new(self.path().to_path_buf()).unwrap()
    }
    fn at_commit(&self) -> String {
        self.out(&["--ignore-working-copy", "log", "-r", "@", "--no-graph", "-T", "commit_id"])
    }
    /// True if a plain `jj status` succeeds (i.e. the working copy is NOT stale).
    fn status_ok(&self) -> bool {
        self.jj(&["status"]).status.success()
    }
    /// Build master + a two-commit stack (botB, topB), then advance master as if
    /// the bottom PR merged. Returns botB's change id (the rebase root).
    fn stack_with_merged_bottom(&self) -> String {
        self.write("base.txt", "b\n");
        self.jj(&["describe", "-m", "BASE"]);
        self.jj(&["bookmark", "create", "master", "-r", "@"]);
        self.jj(&["new", "-m", "BOT"]);
        self.write("bot.txt", "bot\n");
        self.jj(&["status"]);
        self.jj(&["bookmark", "create", "botB", "-r", "@"]);
        let bot_change = self.out(&["log", "-r", "botB", "--no-graph", "-T", "change_id.short(8)"]);
        self.jj(&["new", "-m", "TOP"]);
        self.write("top.txt", "top1\n");
        self.jj(&["status"]);
        self.jj(&["bookmark", "create", "topB", "-r", "@"]);
        // Trunk advances: the bottom PR merged.
        self.jj(&["new", "master", "-m", "MERGED"]);
        self.write("merged.txt", "m\n");
        self.jj(&["status"]);
        self.jj(&["bookmark", "set", "master", "-r", "@"]);
        bot_change
    }
}

/// `@` is on an UNRELATED commit: the rebase must not touch it — same commit id,
/// uncommitted edit preserved, no stale working copy.
#[test]
fn rebase_leaves_unrelated_working_copy_untouched() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let bot_change = r.stack_with_merged_bottom();

    // The user is working on something off master, with an uncommitted edit.
    r.jj(&["new", "master", "-m", "USER_WIP"]);
    r.write("wip.txt", "my-precious-wip\n");
    let at_before = r.at_commit();

    r.runner().rebase_onto(&bot_change, "master").unwrap();

    assert_eq!(r.at_commit(), at_before, "unrelated @ must not move");
    assert!(r.status_ok(), "working copy must not be left stale");
    assert_eq!(
        std::fs::read_to_string(r.path().join("wip.txt")).unwrap(),
        "my-precious-wip\n",
        "the user's uncommitted edit must survive"
    );
    // And the stack actually rebased onto the merged trunk.
    let parent = r.out(&["--ignore-working-copy", "log", "-r", "botB", "--no-graph", "-T", "parents.map(|p| p.description().first_line()).join(\",\")"]);
    assert_eq!(parent, "MERGED", "stack should be rebased onto the merged trunk");
}

/// `@` is ON the stack (the user is editing topB) when the bottom merges: the
/// rebase must update the working copy — no stale error, edit carried along.
#[test]
fn rebase_updates_working_copy_when_user_is_on_the_stack() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let bot_change = r.stack_with_merged_bottom();

    // The user is sitting on topB with an uncommitted edit.
    r.jj(&["edit", "topB"]);
    r.write("top.txt", "top1\nmy-precious-wip\n");

    r.runner().rebase_onto(&bot_change, "master").unwrap();

    assert!(r.status_ok(), "working copy must be updated, not left stale");
    // @ followed the rebase: it is (still) topB, now rebased so botB sits on the
    // merged trunk.
    let at_desc = r.out(&["log", "-r", "@", "--no-graph", "-T", "description.first_line()"]);
    assert_eq!(at_desc, "TOP", "@ should still be the user's TOP commit");
    let bot_parent = r.out(&["log", "-r", "botB", "--no-graph", "-T", "parents.map(|p| p.description().first_line()).join(\",\")"]);
    assert_eq!(bot_parent, "MERGED", "the stack rebased onto the merged trunk");
    // The edit is preserved (in the working copy / commit, not lost).
    assert!(
        std::fs::read_to_string(r.path().join("top.txt")).unwrap().contains("my-precious-wip"),
        "the user's edit must be carried along the rebase"
    );
}

/// Parity with the rebase: the `merge` reconcile strategy must also leave an
/// unrelated working copy untouched — no WIP snapshot, no moved `@`, no stale.
#[test]
fn merge_leaves_unrelated_working_copy_untouched() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.stack_with_merged_bottom();

    r.jj(&["new", "master", "-m", "USER_WIP"]);
    r.write("wip.txt", "my-precious-wip\n");
    let at_before = r.at_commit();

    r.runner().merge_into("topB", "master").unwrap();

    assert_eq!(r.at_commit(), at_before, "unrelated @ must not move/snapshot for a merge");
    assert!(r.status_ok(), "working copy must not be left stale");
    assert_eq!(
        std::fs::read_to_string(r.path().join("wip.txt")).unwrap(),
        "my-precious-wip\n",
        "the user's uncommitted edit must survive"
    );
}

/// jjpr is working-copy-agnostic: read/query operations (what a watch poll runs)
/// must never snapshot the user's WIP into `@`.
#[test]
fn reads_do_not_snapshot_the_working_copy() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.stack_with_merged_bottom();
    r.jj(&["new", "topB", "-m", "USER_WIP"]);
    r.write("scratch.txt", "wip\n");
    let at_before = r.at_commit();
    let runner = r.runner();

    let _ = runner.get_working_copy_commit_id().unwrap();
    let _ = runner.get_git_remotes().unwrap();
    let _ = runner.is_conflicted("@").unwrap();

    assert_eq!(r.at_commit(), at_before, "reads must not snapshot the user's WIP");
    assert!(
        std::fs::read_to_string(r.path().join("scratch.txt")).unwrap().contains("wip"),
        "the edit stays uncommitted on disk"
    );
}

/// A (bottom) <- B (top) <- ongoing UNBOOKMARKED work: when a lower PR merges,
/// jjpr reconciles by pushing the bookmarked segments (B). The ongoing work is
/// structurally invisible to that — it is neither a bookmark nor a stack
/// segment — so it can never be pushed. (The companion stub test
/// `reconcile_proceeds_and_pushes_when_clean` proves reconcile pushes exactly
/// the segment bookmark names; this proves the ongoing work is not among them.)
#[test]
fn unbookmarked_work_above_stack_is_never_a_push_target() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.write("base.txt", "b\n");
    r.jj(&["describe", "-m", "BASE"]);
    r.jj(&["bookmark", "create", "master", "-r", "@"]);
    r.jj(&["new", "-m", "A"]);
    r.write("a.txt", "a\n");
    r.jj(&["status"]);
    r.jj(&["bookmark", "create", "A", "-r", "@"]);
    r.jj(&["new", "-m", "B"]);
    r.write("b.txt", "b\n");
    r.jj(&["status"]);
    r.jj(&["bookmark", "create", "B", "-r", "@"]);
    // Ongoing work on top of B, NOT bookmarked, plus a live uncommitted edit.
    r.jj(&["new", "-m", "ONGOING"]);
    r.write("ongoing.txt", "work\n");
    r.jj(&["status"]);
    r.write("ongoing.txt", "work\nwip\n");
    let ongoing_change = r.out(&["--ignore-working-copy", "log", "-r", "@", "--no-graph", "-T", "change_id"]);
    // The segment map is keyed by COMMIT id (a change id is not unique under
    // divergence), so the "is it a segment" assertion below needs the commit.
    let ongoing_commit = r.out(&["--ignore-working-copy", "log", "-r", "@", "--no-graph", "-T", "commit_id"]);

    let graph = jjpr::graph::change_graph::build_change_graph(&r.runner()).unwrap();

    // The ongoing work carries no bookmark, so it is not a push target...
    assert_eq!(
        r.out(&["--ignore-working-copy", "log", "-r", "@", "--no-graph", "-T", "bookmarks"]),
        "",
        "the ongoing work must be unbookmarked"
    );
    // ...the graph only knows the stack's bookmarks (A, B — never the ongoing
    // work; master may or may not appear depending on trunk resolution)...
    let names: Vec<_> = graph.bookmarks.keys().cloned().collect();
    assert!(
        names.contains(&"A".to_string()) && names.contains(&"B".to_string()),
        "graph should know the stack bookmarks A and B; got {names:?}"
    );
    // ...and the ongoing change is neither a bookmarked change nor a segment.
    assert!(
        !graph.bookmark_to_change_id.values().any(|c| c == &ongoing_change),
        "ongoing work must not be a bookmarked change"
    );
    assert!(
        !graph.commit_id_to_segment.contains_key(&ongoing_commit),
        "ongoing work must not form a stack segment"
    );
}

/// The submit path's explicit snapshot DOES fold the user's current edits into
/// `@` — so a user-invoked command acts on their latest state.
#[test]
fn explicit_snapshot_captures_wip() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.write("base.txt", "b\n");
    r.jj(&["describe", "-m", "A"]);
    let before = r.at_commit();
    r.write("new.txt", "fresh\n");

    r.runner().snapshot().unwrap();

    assert_ne!(r.at_commit(), before, "explicit snapshot should fold WIP into @");
}
