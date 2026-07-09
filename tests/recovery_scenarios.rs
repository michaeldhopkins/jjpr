//! Proven scenarios for the concurrent-modification recovery, run against a
//! real `jj`. These are the *foundation* the recovery policy relies on:
//!
//! - jj's reconcile of two op-log heads preserves both sides' work (it does not
//!   drop commits), so recovery never needs to "replay" — only to avoid the
//!   mangling rebase and to not roll back past the concurrent work.
//! - restoring to the post-fetch operation preserves both works; restoring to
//!   the pre-fetch op discards them. This is why the policy restores only to
//!   post-fetch.
//! - a rebase racing a concurrent reconcile can corrupt (produce divergence),
//!   and restoring to the pre-rebase op recovers the stack intact.
//!
//! If a future jj changes any of these behaviors, these tests fail and tell us
//! the recovery's assumptions shifted. Gated on `jj` being installed.

use std::path::Path;
use std::process::{Command, Output};

use jjpr::jj::{Jj, JjRunner};
use tempfile::TempDir;

fn jj_available() -> bool {
    Command::new("jj").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

struct Repo {
    dir: TempDir,
}
impl Repo {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let ok = Command::new("jj")
            .args(["git", "init"])
            .current_dir(dir.path())
            .output()
            .expect("jj git init")
            .status
            .success();
        assert!(ok, "jj git init failed");
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
    /// A read, run working-copy-agnostically: a concurrent-fork scenario can
    /// leave the working copy stale, and an assertion read must not fail on that
    /// (this is how jjpr reads too, and it keeps the parallel suite from flaking).
    fn out(&self, args: &[&str]) -> String {
        let mut full = vec!["--ignore-working-copy"];
        full.extend_from_slice(args);
        String::from_utf8_lossy(&self.jj(&full).stdout).trim().to_string()
    }
    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path().join(name), content).unwrap();
    }
    fn runner(&self) -> JjRunner {
        JjRunner::new(self.path().to_path_buf()).unwrap()
    }
    fn current_op(&self) -> String {
        self.out(&["op", "log", "-n1", "--no-graph", "-T", "id"])
    }
    /// First-line descriptions of every reachable non-root commit. Read
    /// working-copy-agnostically so a concurrent writer leaving the working copy
    /// stale doesn't block the assertion (exactly how jjpr reads).
    fn descriptions(&self) -> Vec<String> {
        self.out(&["log", "-r", "all() ~ root()", "--no-graph", "-T", r#"description.first_line() ++ "\n""#])
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn reachable(&self, description: &str) -> bool {
        self.descriptions().iter().any(|d| d == description)
    }
    fn divergent(&self) -> Vec<String> {
        self.runner().divergent_change_ids().unwrap()
    }
    fn file_count(&self, revset: &str) -> usize {
        self.out(&["file", "list", "-r", revset]).lines().filter(|l| !l.is_empty()).count()
    }
}

/// jj's reconcile of independent concurrent work keeps BOTH commits — no loss,
/// no divergence. (This is why recovery doesn't need to reconstruct anything.)
#[test]
fn reconcile_preserves_independent_concurrent_work() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.write("base.txt", "b\n");
    r.jj(&["describe", "-m", "A"]);
    let good = r.current_op();

    // Our work.
    r.jj(&["new", "-m", "OUR_WORK"]);
    r.write("our.txt", "o\n");
    r.jj(&["status"]);
    // Their independent work, forked at the same starting op.
    r.jj(&["--at-operation", &good, "new", "-m", "THEIR_WORK"]);
    r.jj(&["status"]); // reconcile

    assert!(r.reachable("OUR_WORK"), "our work lost; got {:?}", r.descriptions());
    assert!(r.reachable("THEIR_WORK"), "their work lost; got {:?}", r.descriptions());
    assert!(r.divergent().is_empty(), "independent work should not be divergent");
}

/// Concurrent edits to the SAME change reconcile to a divergent change — both
/// versions kept. Recovery must preserve both (not pick one blindly, not drop).
#[test]
fn reconcile_preserves_divergent_same_change_edits() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.write("shared.txt", "l1\nl2\n");
    r.jj(&["describe", "-m", "A"]);
    let good = r.current_op();

    r.write("shared.txt", "OUR\nl2\n");
    r.jj(&["status"]);
    r.jj(&["--at-operation", &good, "describe", "-m", "A (their edit)"]);
    r.jj(&["status"]); // reconcile

    assert!(!r.divergent().is_empty(), "same-change edits should reconcile to a divergent change (both kept)");
}

/// Two *equivalent* restacks (same change onto the same base) collapse to a
/// single correct commit — not a divergent change. So the common two-watch race
/// needs no resolution at all.
#[test]
fn equivalent_restacks_collapse_to_one_commit() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.write("base.txt", "b\n");
    r.jj(&["describe", "-m", "BASE"]);
    r.jj(&["bookmark", "create", "master", "-r", "@"]);
    r.jj(&["new", "-m", "S"]);
    r.write("s.txt", "s\n");
    r.jj(&["status"]);
    r.jj(&["bookmark", "create", "feat", "-r", "@"]);
    let s_change = r.out(&["log", "-r", "feat", "--no-graph", "-T", "change_id.short(8)"]);
    // master advances independently.
    r.jj(&["new", "master", "-m", "M"]);
    r.write("m.txt", "m\n");
    r.jj(&["status"]);
    r.jj(&["bookmark", "set", "master", "-r", "@"]);
    let good = r.current_op();

    // Two identical restacks of S onto master.
    r.jj(&["rebase", "-s", &s_change, "-d", "master"]);
    r.jj(&["--at-operation", &good, "rebase", "-s", &s_change, "-d", "master"]);
    r.jj(&["status"]); // reconcile

    let copies = r.out(&["log", "-r", &s_change, "--no-graph", "-T", r#"commit_id ++ "\n""#]);
    assert_eq!(copies.lines().filter(|l| !l.is_empty()).count(), 1, "equivalent restacks should collapse to one commit");
    assert!(r.divergent().is_empty(), "equivalent restacks should not be divergent");
    // The single commit carries both S's file and master's file.
    assert_eq!(r.file_count(&s_change), 3, "collapsed commit should have base.txt + m.txt + s.txt");
}

/// THE central proof: restoring to the post-fetch op preserves both sides' work,
/// while restoring to the pre-fetch (`good`) op discards both. This is why the
/// policy restores only to post-fetch — never past it.
#[test]
fn restore_to_post_fetch_preserves_work_but_good_op_loses_it() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.write("base.txt", "b\n");
    r.jj(&["describe", "-m", "A"]);
    let good = r.current_op();

    r.jj(&["new", "-m", "OUR_WORK"]);
    r.jj(&["--at-operation", &good, "new", "-m", "THEIR_WORK"]);
    r.jj(&["status"]); // reconcile
    let post_fetch = r.current_op();

    assert!(r.reachable("OUR_WORK") && r.reachable("THEIR_WORK"), "reconciled state should hold both");

    let runner = r.runner();
    // Rolling back to the pre-fetch op discards BOTH works from the view.
    runner.restore_operation(&good).unwrap();
    assert!(
        !r.reachable("OUR_WORK") && !r.reachable("THEIR_WORK"),
        "good_op restore discards both works; got {:?}",
        r.descriptions()
    );
    // Restoring to the post-fetch op brings both back (op restore is reversible).
    runner.restore_operation(&post_fetch).unwrap();
    assert!(
        r.reachable("OUR_WORK") && r.reachable("THEIR_WORK"),
        "post-fetch restore must preserve both works; got {:?}",
        r.descriptions()
    );
}

/// A rebase racing a concurrent reconcile can corrupt the stack (produce a
/// divergent change). Restoring to the pre-rebase (post-fetch) op recovers the
/// stack intact — same file count, no divergence.
#[test]
fn rebase_race_corrupts_and_restore_to_post_fetch_recovers() {
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
    r.jj(&["bookmark", "create", "botB", "-r", "@"]);
    r.jj(&["new", "-m", "B"]);
    for f in ["b1", "b2", "b3"] {
        r.write(&format!("{f}.txt"), "x\n");
    }
    r.jj(&["status"]);
    r.jj(&["bookmark", "create", "topB", "-r", "@"]);
    let b_change = r.out(&["log", "-r", "topB", "--no-graph", "-T", "change_id.short(8)"]);

    // Simulate the fetch importing a squash-merge of A into master.
    r.jj(&["new", "master", "-m", "squash A"]);
    r.write("a.txt", "a\n");
    r.jj(&["status"]);
    r.jj(&["bookmark", "set", "master", "-r", "@"]);
    let post_fetch = r.current_op();
    let files_before = r.file_count("topB");

    // The race: our rebase of B onto the squashed master vs. a fork abandoning A.
    r.jj(&["rebase", "-s", &b_change, "-d", "master"]);
    r.jj(&["--at-operation", &post_fetch, "abandon", "botB"]);
    r.jj(&["status"]); // reconcile

    assert!(!r.divergent().is_empty(), "the rebase race should have corrupted B into a divergent change");

    // Recovery: restore to the pre-rebase (post-fetch) op.
    r.runner().restore_operation(&post_fetch).unwrap();
    assert!(r.divergent().is_empty(), "restore to post-fetch should clear the divergence");
    assert_eq!(r.file_count("topB"), files_before, "B's files must all be intact after recovery");
}

/// The load-bearing guarantee for a `divergent()`-only gate: proceeding through a
/// concurrent reconcile that produced NO divergence (independent work) does not
/// corrupt — the stack rebase stays clean, files intact, both works preserved.
/// This is why gating on `divergent()` alone is sufficient: corruption shows up
/// as divergence, and a non-divergent reconcile is safe to proceed through.
#[test]
fn proceeding_through_a_nondivergent_reconcile_stays_clean() {
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
    for f in ["b1", "b2", "b3"] {
        r.write(&format!("{f}.txt"), "x\n");
    }
    r.jj(&["status"]);
    r.jj(&["bookmark", "create", "B", "-r", "@"]);
    let b_change = r.out(&["log", "-r", "B", "--no-graph", "-T", "change_id.short(8)"]);
    let op0 = r.current_op();
    let b_files_before = r.file_count("B");

    // The fetch imports A's squash-merge into the trunk.
    r.jj(&["--ignore-working-copy", "new", "--no-edit", "master", "-m", "A-squash"]);
    r.jj(&["--ignore-working-copy", "bookmark", "set", "master", "-r", "description(\"A-squash\")"]);
    // A concurrent process, forked before the squash, adds INDEPENDENT work —
    // this forks the op log; the next command reconciles the two heads.
    r.jj(&["--ignore-working-copy", "--at-operation", &op0, "new", "master", "-m", "INDEP"]);
    r.jj(&["--ignore-working-copy", "log", "-r", "@"]);
    assert!(r.divergent().is_empty(), "an independent concurrent reconcile must not diverge");

    // jjpr proceeds: rebase the remaining stack onto the new trunk.
    r.jj(&["rebase", "-s", &b_change, "-d", "master"]);

    assert!(
        r.divergent().is_empty(),
        "proceeding through a non-divergent reconcile must stay clean; got divergence {:?}",
        r.divergent()
    );
    assert_eq!(r.file_count("B"), b_files_before, "B's files must be intact");
    assert!(r.reachable("INDEP"), "independent concurrent work must be preserved; got {:?}", r.descriptions());
    assert!(r.reachable("B"), "B must be preserved; got {:?}", r.descriptions());
}
