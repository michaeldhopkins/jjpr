//! A fetch that brings in a rewritten commit under the checkout.
//!
//! jj rebases what sits on a commit the fetch rewrote, `@` included. Fetched
//! with `--ignore-working-copy`, the checkout is left behind on disk: the next
//! jj command finds the working copy stale, and taking it into account then
//! gives `@` a second, divergent copy. A merge reconcile read that divergence as
//! a concurrent jj process. submit and merge have already snapshotted the
//! checkout, so their fetch updates it. watch, which never touches the
//! checkout, still fetches without it.

mod common;

use std::path::Path;
use std::process::Command;

use common::JjTestRepo;
use jjpr::jj::Jj;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=Other", "-c", "user.email=other@jjpr.dev"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A repo whose checkout sits on `feat`, which origin has since rewritten.
fn checkout_on_a_rewritten_bookmark() -> Option<(JjTestRepo, tempfile::TempDir)> {
    if !common::jj_available() {
        return None;
    }
    let repo = JjTestRepo::new();
    repo.commit_and_bookmark("feat.rs", "feat\n", "feature", "feat");
    let mut push =
        jjpr::jj::version::push_new_bookmark_args(jjpr::jj::version::installed_jj_version())
            .to_vec();
    push.extend(["git", "push", "--remote", "origin", "--bookmark", "feat"]);
    repo.run_jj(&push);
    repo.write_file("wip.rs", "uncommitted\n");

    let other = tempfile::TempDir::new().expect("temp dir");
    let origin = repo.origin_path().to_str().expect("utf-8 path");
    git(other.path(), &["clone", "-q", origin, "c"]);
    let clone = other.path().join("c");
    git(&clone, &["checkout", "-q", "feat"]);
    git(
        &clone,
        &["commit", "-q", "--amend", "-m", "feature, reworded"],
    );
    git(&clone, &["push", "-q", "-f", "origin", "feat"]);
    Some((repo, other))
}

fn status(repo: &JjTestRepo) -> std::process::Output {
    Command::new("jj")
        .args(["status"])
        .current_dir(repo.path())
        .output()
        .expect("run jj")
}

/// Discriminates on jj 0.36 through 0.38, which CI's `jj-versions` job runs:
/// later jj recovers the stale checkout by itself, so this passes either way.
#[test]
fn a_fetch_after_a_snapshot_keeps_the_checkout_current() {
    let Some((repo, _other)) = checkout_on_a_rewritten_bookmark() else {
        return;
    };
    let jj = repo.runner();
    jj.snapshot().unwrap();
    jj.git_fetch().unwrap();

    let out = status(&repo);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("stale"), "{stderr}");
    let change = repo.run_jj(&["log", "--no-graph", "-r", "@", "-T", "change_id"]);
    let all_copies = format!("change_id({})", change.trim());
    let copies = repo.run_jj(&[
        "log",
        "--no-graph",
        "-r",
        &all_copies,
        "-T",
        r#"commit_id ++ "\n""#,
    ]);
    assert_eq!(copies.lines().count(), 1, "@ is not divergent: {copies}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("wip.rs")).unwrap(),
        "uncommitted\n"
    );
    let in_checkout = repo.run_jj(&["diff", "--name-only", "-r", "@"]);
    assert!(in_checkout.lines().any(|f| f == "wip.rs"), "{in_checkout}");
}

#[test]
fn a_fetch_without_a_snapshot_leaves_the_checkout_alone() {
    let Some((repo, _other)) = checkout_on_a_rewritten_bookmark() else {
        return;
    };
    // The checkout file records the operation the files on disk match. A
    // fetch that updates the checkout rewrites it; one that ignores it cannot.
    let checkout = repo.path().join(".jj/working_copy/checkout");
    let before = std::fs::read(&checkout).expect("jj keeps a checkout file");
    repo.runner().git_fetch().unwrap();
    let after = std::fs::read(&checkout).expect("jj keeps a checkout file");
    assert_eq!(before, after, "watch's fetch must not touch the checkout");
}

#[test]
fn the_checkouts_parents_change_when_a_fetch_rewrites_them() {
    let Some((repo, _other)) = checkout_on_a_rewritten_bookmark() else {
        return;
    };
    let jj = repo.runner();
    jj.snapshot().unwrap();
    let before = jj.working_copy_view().unwrap().parents;
    assert_eq!(before.len(), 1, "{before:?}");
    jj.git_fetch().unwrap();
    let after = jj.working_copy_view().unwrap().parents;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_ne!(before, after, "the fetch rewrote the commit @ sits on");
    assert_eq!(
        jj.working_copy_view().unwrap().parents,
        after,
        "and a read is stable"
    );
}
