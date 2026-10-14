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

mod common;
use common::jj_available;

/// Pinned commit timestamps. Two distinct values so a test can choose whether
/// two operations produce identical commits or merely equivalent ones.
const FIXED_TS: &str = "2001-02-03T04:05:06+07:00";
const LATER_TS: &str = "2001-02-03T04:09:09+07:00";

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
        self.jj_inner(args, None)
    }
    /// Run with a pinned commit timestamp.
    ///
    /// A commit's identity includes its committer timestamp, so two operations
    /// that are "the same" but land either side of a clock tick produce
    /// *different* commits — and jj records that as a divergent change rather
    /// than collapsing them. Pinning the clock is what lets a test choose
    /// deterministically between the two, instead of racing a second boundary.
    ///
    /// `JJ_TIMESTAMP` is jj's mapping onto the `debug.commit-timestamp` config
    /// key. It is a debug knob, not a stability-guaranteed interface: an
    /// unparseable value fails loudly, but if a future jj simply stopped reading
    /// the variable it would be ignored in silence and these tests would quietly
    /// go back to being timing-dependent. `assert_timestamp_pin_works` guards
    /// that, so the mechanism failing is a failure rather than a flake.
    fn jj_at(&self, timestamp: &str, args: &[&str]) -> Output {
        self.jj_inner(args, Some(timestamp))
    }
    fn jj_inner(&self, args: &[&str], timestamp: Option<&str>) -> Output {
        let mut cmd = Command::new("jj");
        cmd.args(["--config=user.name=T", "--config=user.email=t@e.com"])
            .args(args)
            .current_dir(self.path());
        if let Some(ts) = timestamp {
            cmd.env("JJ_TIMESTAMP", ts);
        }
        cmd.output().expect("jj command")
    }
    /// A read, run working-copy-agnostically: a concurrent-fork scenario can
    /// leave the working copy stale, and an assertion read must not fail on that
    /// (this is how jjpr reads too, and it keeps the parallel suite from flaking).
    ///
    /// Fails loudly when the command does. Returning stdout alone turns any
    /// error into an empty string, which then fails some unrelated assertion as
    /// a wrong *value* — e.g. `jj log -r <id>` errors on a divergent change ID,
    /// which surfaced as "expected 1 commit, got 0" and hid the real cause.
    fn out(&self, args: &[&str]) -> String {
        let mut full = vec!["--ignore-working-copy"];
        full.extend_from_slice(args);
        let output = self.jj(&full);
        assert!(
            output.status.success(),
            "jj {full:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path().join(name), content).expect("write a file in the test repo");
    }
    fn runner(&self) -> JjRunner {
        JjRunner::new(self.path().to_path_buf()).expect("open the test repo")
    }
    fn current_op(&self) -> String {
        self.out(&["op", "log", "-n1", "--no-graph", "-T", "id"])
    }
    /// First-line descriptions of every reachable non-root commit. Read
    /// working-copy-agnostically so a concurrent writer leaving the working copy
    /// stale doesn't block the assertion (exactly how jjpr reads).
    fn descriptions(&self) -> Vec<String> {
        self.out(&[
            "log",
            "-r",
            "all() ~ root()",
            "--no-graph",
            "-T",
            r#"description.first_line() ++ "\n""#,
        ])
        .lines()
        .map(str::to_string)
        .collect()
    }
    fn reachable(&self, description: &str) -> bool {
        self.descriptions().iter().any(|d| d == description)
    }
    fn divergent(&self) -> Vec<String> {
        self.runner()
            .divergent_change_ids()
            .expect("read divergent change ids")
    }
    fn file_count(&self, revset: &str) -> usize {
        self.out(&["file", "list", "-r", revset])
            .lines()
            .filter(|l| !l.is_empty())
            .count()
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

    assert!(
        r.reachable("OUR_WORK"),
        "our work lost; got {:?}",
        r.descriptions()
    );
    assert!(
        r.reachable("THEIR_WORK"),
        "their work lost; got {:?}",
        r.descriptions()
    );
    assert!(
        r.divergent().is_empty(),
        "independent work should not be divergent"
    );
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

    assert!(
        !r.divergent().is_empty(),
        "same-change edits should reconcile to a divergent change (both kept)"
    );
}

/// Two *byte-identical* restacks (same change, same base, same commit
/// timestamp) collapse to a single commit rather than a divergent change.
///
/// Read the precondition carefully: this is only reachable when both rebases
/// produce the same commit, which requires the same committer timestamp. It is
/// **not** a model of the two-watch race — see
/// `restacks_a_realistic_race_apart_in_time_do_diverge`, which is the case
/// jjpr actually has to survive. This test covers only the narrow mechanism:
/// when jj sees the same commit twice, it does not manufacture divergence.
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
    let s_change = r.out(&[
        "log",
        "-r",
        "feat",
        "--no-graph",
        "-T",
        "change_id.short(8)",
    ]);
    // master advances independently.
    r.jj(&["new", "master", "-m", "M"]);
    r.write("m.txt", "m\n");
    r.jj(&["status"]);
    r.jj(&["bookmark", "set", "master", "-r", "@"]);
    let good = r.current_op();

    // Both rebases pinned to the SAME timestamp, so the two commits really are
    // identical. On the wall clock they collapse only when both land inside the
    // same second: measured 10/100 divergent back-to-back, and 12/12 divergent
    // with a 2-second gap.
    r.jj_at(FIXED_TS, &["rebase", "-s", &s_change, "-d", "master"]);
    r.jj_at(
        FIXED_TS,
        &[
            "--at-operation",
            &good,
            "rebase",
            "-s",
            &s_change,
            "-d",
            "master",
        ],
    );
    r.jj(&["status"]); // reconcile

    let copies = r.out(&[
        "log",
        "-r",
        &s_change,
        "--no-graph",
        "-T",
        r#"commit_id ++ "\n""#,
    ]);
    assert_eq!(
        copies.lines().filter(|l| !l.is_empty()).count(),
        1,
        "equivalent restacks should collapse to one commit"
    );
    assert!(
        r.divergent().is_empty(),
        "equivalent restacks should not be divergent"
    );
    // The single commit carries both S's file and master's file.
    assert_eq!(
        r.file_count(&s_change),
        3,
        "collapsed commit should have base.txt + m.txt + s.txt"
    );
}

/// The realistic two-watch race: the same restack performed twice, seconds
/// apart, **does** produce a divergent change.
///
/// This is the case that matters. Two `jjpr watch` processes poll 30s apart, so
/// their rebases never share a committer timestamp and their commits are never
/// identical. Measured on the wall clock: 12/12 divergent at a 2-second gap.
/// The timestamps here are pinned to two distinct values purely to get that
/// determinism without a `sleep`.
///
/// So jjpr's divergence gating is the load-bearing path for concurrent watches,
/// not a rare fallback — which is why `reconcile_local_state` checks
/// `divergent_change_ids` three times and never rebases on top of a divergent
/// stack.
#[test]
fn restacks_a_realistic_race_apart_in_time_do_diverge() {
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
    let s_change = r.out(&[
        "log",
        "-r",
        "feat",
        "--no-graph",
        "-T",
        "change_id.short(8)",
    ]);
    r.jj(&["new", "master", "-m", "M"]);
    r.write("m.txt", "m\n");
    r.jj(&["status"]);
    r.jj(&["bookmark", "set", "master", "-r", "@"]);
    let good = r.current_op();

    // Same restack twice, but at different clock times — as two watch processes
    // would inevitably do.
    r.jj_at(FIXED_TS, &["rebase", "-s", &s_change, "-d", "master"]);
    r.jj_at(
        LATER_TS,
        &[
            "--at-operation",
            &good,
            "rebase",
            "-s",
            &s_change,
            "-d",
            "master",
        ],
    );
    r.jj(&["status"]); // reconcile

    assert!(
        !r.divergent().is_empty(),
        "restacks at different times must diverge; if this ever passes, the \
         claim that concurrent watches self-heal needs re-examining"
    );
    assert!(
        r.divergent().iter().any(|c| c.starts_with(&s_change)),
        "the divergent change should be the restacked one: {:?}",
        r.divergent()
    );
}

/// Guards the pinning mechanism itself.
///
/// `JJ_TIMESTAMP` is a debug knob (it maps to `debug.commit-timestamp`). If a
/// future jj stopped honouring it, nothing would error — the variable would
/// just be ignored, and the two tests above would silently revert to racing the
/// clock, one of them flaking ~10% of runs and the other passing for the wrong
/// reason. Assert the pin actually lands, so that regression is a failure.
#[test]
fn assert_timestamp_pin_works() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    r.write("f.txt", "x\n");
    r.jj_at(FIXED_TS, &["describe", "-m", "PINNED"]);
    let committed = r.out(&[
        "log",
        "-r",
        "@",
        "--no-graph",
        "-T",
        "committer.timestamp()",
    ]);
    assert!(
        committed.starts_with("2001-02-03"),
        "JJ_TIMESTAMP is no longer honoured (got {committed:?}); the determinism \
         in this suite depends on it"
    );
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

    assert!(
        r.reachable("OUR_WORK") && r.reachable("THEIR_WORK"),
        "reconciled state should hold both"
    );

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
    let b_change = r.out(&[
        "log",
        "-r",
        "topB",
        "--no-graph",
        "-T",
        "change_id.short(8)",
    ]);

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

    assert!(
        !r.divergent().is_empty(),
        "the rebase race should have corrupted B into a divergent change"
    );

    // Recovery: restore to the pre-rebase (post-fetch) op.
    r.runner().restore_operation(&post_fetch).unwrap();
    assert!(
        r.divergent().is_empty(),
        "restore to post-fetch should clear the divergence"
    );
    assert_eq!(
        r.file_count("topB"),
        files_before,
        "B's files must all be intact after recovery"
    );
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
    r.jj(&[
        "--ignore-working-copy",
        "new",
        "--no-edit",
        "master",
        "-m",
        "A-squash",
    ]);
    r.jj(&[
        "--ignore-working-copy",
        "bookmark",
        "set",
        "master",
        "-r",
        "description(\"A-squash\")",
    ]);
    // A concurrent process, forked before the squash, adds INDEPENDENT work —
    // this forks the op log; the next command reconciles the two heads.
    r.jj(&[
        "--ignore-working-copy",
        "--at-operation",
        &op0,
        "new",
        "master",
        "-m",
        "INDEP",
    ]);
    r.jj(&["--ignore-working-copy", "log", "-r", "@"]);
    assert!(
        r.divergent().is_empty(),
        "an independent concurrent reconcile must not diverge"
    );

    // jjpr proceeds: rebase the remaining stack onto the new trunk.
    r.jj(&["rebase", "-s", &b_change, "-d", "master"]);

    assert!(
        r.divergent().is_empty(),
        "proceeding through a non-divergent reconcile must stay clean; got divergence {:?}",
        r.divergent()
    );
    assert_eq!(
        r.file_count("B"),
        b_files_before,
        "B's files must be intact"
    );
    assert!(
        r.reachable("INDEP"),
        "independent concurrent work must be preserved; got {:?}",
        r.descriptions()
    );
    assert!(
        r.reachable("B"),
        "B must be preserved; got {:?}",
        r.descriptions()
    );
}

/// A stack with `@` on its top segment, the bottom squash-merged into master,
/// and an edit on disk that no jj command has snapshotted yet. Returns the
/// top segment's root change id.
fn stack_with_unsnapshotted_edit(r: &Repo) -> String {
    r.write("base.txt", "b\n");
    r.jj(&["describe", "-m", "BASE"]);
    r.jj(&["bookmark", "create", "master", "-r", "@"]);
    r.jj(&["new", "-m", "A"]);
    r.write("a.txt", "a\n");
    r.jj(&["bookmark", "create", "botA", "-r", "@"]);
    r.jj(&["new", "-m", "B"]);
    r.write("b.txt", "b\n");
    r.jj(&["bookmark", "create", "topB", "-r", "@"]);
    let b_change = r.out(&[
        "log",
        "-r",
        "topB",
        "--no-graph",
        "-T",
        "change_id.short(8)",
    ]);
    r.jj(&["new", "master", "-m", "squash A"]);
    r.write("a.txt", "a\n");
    r.jj(&["bookmark", "set", "master", "-r", "@"]);
    r.jj(&["edit", "topB"]);
    r.write("wip.txt", "unsaved\n");
    b_change
}

fn wip_on_disk(r: &Repo) -> Option<String> {
    std::fs::read_to_string(r.path().join("wip.txt")).ok()
}

/// The premise of snapshotting before the restore point: a rebase that moves
/// `@` snapshots the edit into its own operation, so restoring to an operation
/// captured before that snapshot takes the edit off the disk.
#[test]
fn restoring_past_the_rebase_snapshot_loses_an_unsnapshotted_edit() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let b_change = stack_with_unsnapshotted_edit(&r);
    let runner = r.runner();
    let post_fetch = r.current_op();
    runner.rebase_onto(&b_change, "master").unwrap();
    runner.restore_operation(&post_fetch).unwrap();
    assert_eq!(
        wip_on_disk(&r),
        None,
        "jj changed: the edit survived the restore"
    );
}

/// With the subtree snapshotted before the restore point is captured, rolling
/// the rebase back keeps the user's edit.
#[test]
fn snapshotting_the_moved_subtree_first_keeps_the_edit_through_a_restore() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let b_change = stack_with_unsnapshotted_edit(&r);
    let runner = r.runner();
    assert!(runner.snapshot_if_moved_by(&b_change).unwrap());
    let post_fetch = r.current_op();
    runner.rebase_onto(&b_change, "master").unwrap();
    runner.restore_operation(&post_fetch).unwrap();
    assert_eq!(wip_on_disk(&r).as_deref(), Some("unsaved\n"));
}

/// `@` outside the rebased subtree: nothing is snapshotted, so the operation
/// log does not move.
#[test]
fn snapshot_if_moved_by_leaves_an_unrelated_checkout_alone() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let b_change = stack_with_unsnapshotted_edit(&r);
    r.jj(&["new", "master"]);
    r.write("other.txt", "elsewhere\n");
    let before = r.current_op();
    assert!(!r.runner().snapshot_if_moved_by(&b_change).unwrap());
    assert_eq!(r.current_op(), before);
}

/// A scope jj cannot evaluate counts as reaching `@`: snapshotting needlessly
/// costs an operation, while skipping a needed one loses the edit.
#[test]
fn an_unreadable_scope_snapshots_anyway() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    stack_with_unsnapshotted_edit(&r);
    let before = r.current_op();
    assert!(
        r.runner()
            .snapshot_if_moved_by("no_such_bookmark_zz")
            .unwrap()
    );
    assert_ne!(r.current_op(), before);
}

/// `@` outside the rebased subtree, with an unsnapshotted edit. A plain restore
/// snapshots the edit into a new operation and then checks out the restored
/// `@`, which never had it.
#[test]
fn a_plain_restore_loses_an_edit_outside_the_rebased_subtree() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let b_change = stack_with_unsnapshotted_edit(&r);
    r.jj(&["new", "master"]);
    r.write("other.txt", "elsewhere\n");
    let runner = r.runner();
    assert!(!runner.snapshot_if_moved_by(&b_change).unwrap());
    let post_fetch = r.current_op();
    runner.rebase_onto(&b_change, "master").unwrap();
    runner.restore_operation(&post_fetch).unwrap();
    assert!(
        !r.path().join("other.txt").exists(),
        "jj changed: the edit survived a plain restore"
    );
}

/// The same restore leaving the working copy alone keeps the edit, and the
/// next jj command snapshots it into `@` as usual.
#[test]
fn restoring_without_touching_the_working_copy_keeps_an_edit_outside_the_subtree() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let b_change = stack_with_unsnapshotted_edit(&r);
    r.jj(&["new", "master"]);
    r.write("other.txt", "elsewhere\n");
    let runner = r.runner();
    assert!(!runner.snapshot_if_moved_by(&b_change).unwrap());
    let post_fetch = r.current_op();
    runner.rebase_onto(&b_change, "master").unwrap();
    runner
        .restore_operation_keeping_working_copy(&post_fetch)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(r.path().join("other.txt"))
            .ok()
            .as_deref(),
        Some("elsewhere\n")
    );
    let status = r.jj(&["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let files = r.out(&["diff", "--name-only", "-r", "@"]);
    assert_eq!(files.trim(), "other.txt");
}

/// The rewrite decides for itself whether to touch the working copy. When it
/// does after the restore point said it would not (a failed check falls back to
/// touching it), the edit is recorded only in a later `@`; restoring around the
/// working copy would leave it stale, and updating it would take the edit off
/// the disk. The restore refuses instead, and the edit stays where it is.
#[test]
fn a_restore_around_the_working_copy_refuses_once_the_checkout_has_moved() {
    if !jj_available() {
        return;
    }
    let r = Repo::new();
    let b_change = stack_with_unsnapshotted_edit(&r);
    r.jj(&["new", "master"]);
    r.write("other.txt", "elsewhere\n");
    let runner = r.runner();
    assert!(!runner.snapshot_if_moved_by(&b_change).unwrap());
    let post_fetch = r.current_op();
    r.jj(&["rebase", "-s", &b_change, "-d", "master"]);
    let err = runner
        .restore_operation_keeping_working_copy(&post_fetch)
        .unwrap_err();
    assert!(err.to_string().contains("working copy"), "got {err}");
    assert_eq!(
        std::fs::read_to_string(r.path().join("other.txt"))
            .ok()
            .as_deref(),
        Some("elsewhere\n")
    );
    let status = r.jj(&["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
}
