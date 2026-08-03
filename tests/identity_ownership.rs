//! Real-jj proof that the ownership revset (`Identity::owned_revset`, wired
//! through `JjRunner::set_identity`) scopes discovery to ALL of your commit
//! emails — the multi-machine / multi-email case. Without it, work authored
//! under a second email is invisible to every command.
//!
//! Gated on `jj` being installed.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use jjpr::identity::Identity;
use jjpr::jj::types::Bookmark;
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
        let repo = Self { dir };
        repo.run(&["git", "init"]);
        repo.run(&["config", "set", "--repo", "user.name", "Me"]);
        repo.run(&[
            "config",
            "set",
            "--repo",
            "user.email",
            "me@example.invalid",
        ]);
        repo
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn run(&self, args: &[&str]) {
        let ok = Command::new("jj")
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("jj command")
            .status
            .success();
        assert!(ok, "jj {args:?} failed");
    }
}

fn names(bookmarks: Vec<Bookmark>) -> HashSet<String> {
    bookmarks.into_iter().map(|b| b.name).collect()
}

fn id(emails: &[&str]) -> Identity {
    Identity {
        emails: emails.iter().map(|s| s.to_string()).collect(),
        logins: vec![],
    }
}

/// Two sibling bookmarks authored under two different emails.
fn two_email_repo() -> Repo {
    let repo = Repo::new();
    repo.run(&["new", "root()", "--config=user.email=a@x.com", "-m", "A"]);
    repo.run(&["bookmark", "set", "bmA", "-r", "@"]);
    repo.run(&["new", "root()", "--config=user.email=b@x.com", "-m", "B"]);
    repo.run(&["bookmark", "set", "bmB", "-r", "@"]);
    repo
}

#[test]
fn owned_revset_scopes_get_my_bookmarks() {
    if !jj_available() {
        return;
    }
    let repo = two_email_repo();
    let mut jj = JjRunner::new(repo.path().to_path_buf()).expect("runner");

    // One email → only that email's bookmark (submit/watch/merge discovery).
    jj.set_identity(&id(&["a@x.com"]));
    let a_only = names(jj.get_my_bookmarks().expect("get_my_bookmarks"));
    assert!(a_only.contains("bmA"), "{a_only:?}");
    assert!(
        !a_only.contains("bmB"),
        "one-email discovery must exclude the other: {a_only:?}"
    );

    // Both emails → both bookmarks: your second-machine work is now yours.
    jj.set_identity(&id(&["a@x.com", "b@x.com"]));
    let both = names(jj.get_my_bookmarks().expect("get_my_bookmarks"));
    assert!(
        both.contains("bmA") && both.contains("bmB"),
        "union must include both: {both:?}"
    );
}

#[test]
fn owned_revset_scopes_broad_status_discovery() {
    if !jj_available() {
        return;
    }
    let repo = two_email_repo();
    // Park the working copy on root so `::@` contributes nothing above trunk —
    // this isolates the `owned()` half of the broad status revset.
    repo.run(&["new", "root()"]);
    let mut jj = JjRunner::new(repo.path().to_path_buf()).expect("runner");

    jj.set_identity(&id(&["a@x.com", "b@x.com"]));
    let both = names(jj.get_status_bookmarks(true).expect("get_status_bookmarks"));
    assert!(
        both.contains("bmA") && both.contains("bmB"),
        "broad status union: {both:?}"
    );

    jj.set_identity(&id(&["a@x.com"]));
    let a_only = names(jj.get_status_bookmarks(true).expect("get_status_bookmarks"));
    assert!(
        a_only.contains("bmA") && !a_only.contains("bmB"),
        "broad status scoped: {a_only:?}"
    );
}

/// Regression: the default identity (never calling `set_identity`) behaves like
/// the pre-change `mine()` — commits authored by the configured local email.
#[test]
fn default_identity_matches_local_email_only() {
    if !jj_available() {
        return;
    }
    let repo = Repo::new();
    // Authored under the repo's own user.email (the resolved local identity).
    repo.run(&["new", "root()", "-m", "mine"]);
    repo.run(&["bookmark", "set", "local", "-r", "@"]);
    repo.run(&[
        "new",
        "root()",
        "--config=user.email=other@x.com",
        "-m",
        "other",
    ]);
    repo.run(&["bookmark", "set", "foreign", "-r", "@"]);

    let jj = JjRunner::new(repo.path().to_path_buf()).expect("runner");
    // No set_identity → owned_revset stays `mine()`: the local-email bookmark is
    // yours, the other-email one is not — byte-for-byte the prior behavior.
    let mine = names(jj.get_my_bookmarks().expect("get_my_bookmarks"));
    assert!(
        mine.contains("local"),
        "default mine() must include local-email work: {mine:?}"
    );
    assert!(
        !mine.contains("foreign"),
        "default mine() must exclude other-email work: {mine:?}"
    );
}
