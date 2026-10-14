//! Bookmark names that jj would read as something else, against a real jj.
//!
//! The repo holds a remote bookmark `foo@origin` (the branch `foo` on origin)
//! and, at a different commit, a local bookmark whose name is `foo@origin`.
//! Written bare, the name is the remote bookmark; jjpr must always mean the
//! local one. Each test drives one place jjpr hands a name to jj.

mod common;

use common::JjTestRepo;
use jjpr::jj::{Jj, revset};
use jjpr::undo::repo::{JjRepo, UndoRepo};

const TRICKY: &str = "foo@origin";

struct Fixture {
    repo: JjTestRepo,
    /// The commit the remote bookmark `foo@origin` points at.
    remote_foo: String,
    /// The commit the local bookmark named `foo@origin` points at.
    local_tricky: String,
}

fn commit_of(repo: &JjTestRepo, rev: &str) -> String {
    repo.run_jj(&["log", "--no-graph", "-r", rev, "-T", "commit_id"])
        .trim()
        .to_string()
}

fn parents_of(repo: &JjTestRepo, rev: &str) -> Vec<String> {
    let out = repo.run_jj(&[
        "log",
        "--no-graph",
        "-r",
        rev,
        "-T",
        r#"parents.map(|p| p.commit_id()).join("\n")"#,
    ]);
    let mut ids: Vec<String> = out.lines().map(String::from).collect();
    ids.sort();
    ids
}

fn fixture() -> Option<Fixture> {
    if !common::jj_available() {
        return None;
    }
    let repo = JjTestRepo::new();
    repo.commit_and_bookmark("remote.rs", "remote\n", "remote foo", "foo");
    let mut push =
        jjpr::jj::version::push_new_bookmark_args(jjpr::jj::version::installed_jj_version())
            .to_vec();
    push.extend(["git", "push", "--remote", "origin", "--bookmark", "foo"]);
    repo.run_jj(&push);
    let remote_foo = commit_of(&repo, "foo@origin");

    repo.run_jj(&["new", "main", "-m", "local tricky"]);
    repo.write_file("tricky.rs", "tricky\n");
    repo.run_jj(&["commit", "-m", "local tricky"]);
    repo.run_jj(&["bookmark", "create", r#""foo@origin""#, "-r", "@-"]);
    let local_tricky = commit_of(&repo, r#""foo@origin""#);
    assert_ne!(remote_foo, local_tricky);

    repo.run_jj(&["new", "main", "-m", "feature"]);
    repo.write_file("feat.rs", "feat\n");
    repo.run_jj(&["commit", "-m", "feature"]);
    repo.set_bookmark("feat");
    Some(Fixture {
        repo,
        remote_foo,
        local_tricky,
    })
}

fn change_of(repo: &JjTestRepo, rev: &str) -> String {
    repo.run_jj(&["log", "--no-graph", "-r", rev, "-T", "change_id"])
        .trim()
        .to_string()
}

#[test]
fn a_bare_name_would_have_meant_the_remote_bookmark() {
    let Some(f) = fixture() else { return };
    assert_eq!(commit_of(&f.repo, TRICKY), f.remote_foo);
    assert_eq!(commit_of(&f.repo, &revset::symbol(TRICKY)), f.local_tricky);
}

#[test]
fn the_bookmark_list_reports_the_name_unquoted() {
    let Some(f) = fixture() else { return };
    let names: Vec<String> = f
        .repo
        .runner()
        .get_status_bookmarks(true)
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert!(names.contains(&TRICKY.to_string()), "{names:?}");
}

#[test]
fn rebase_onto_a_named_base_lands_on_the_local_bookmark() {
    let Some(f) = fixture() else { return };
    let jj = f.repo.runner();
    let feat = revset::change_id(&change_of(&f.repo, "feat"));
    let base = revset::symbol(TRICKY);
    jj.rebase_onto(&feat, &base).unwrap();
    assert_eq!(parents_of(&f.repo, "feat"), vec![f.local_tricky.clone()]);
    assert!(jj.is_rooted_in(&feat, &base).unwrap());
    assert!(
        !jj.is_rooted_in(&feat, TRICKY).unwrap(),
        "bare, the name is the remote bookmark, which feat is not on"
    );
    assert!(
        !jj.is_conflicted(&revset::dag_range(&feat, &revset::symbol("feat")))
            .unwrap()
    );
}

#[test]
fn merge_into_a_named_base_merges_the_local_bookmark() {
    let Some(f) = fixture() else { return };
    let before = commit_of(&f.repo, "feat");
    f.repo.runner().merge_into("feat", TRICKY).unwrap();
    let mut want = vec![before, f.local_tricky.clone()];
    want.sort();
    assert_eq!(parents_of(&f.repo, "feat"), want);
}

#[test]
fn merge_into_moves_a_bookmark_with_a_tricky_name() {
    let Some(f) = fixture() else { return };
    f.repo.runner().merge_into(TRICKY, "feat").unwrap();
    let mut want = vec![f.local_tricky.clone(), commit_of(&f.repo, "feat")];
    want.sort();
    assert_eq!(parents_of(&f.repo, &revset::symbol(TRICKY)), want);
}

#[test]
fn forget_takes_only_the_bookmark_of_that_name() {
    let Some(f) = fixture() else { return };
    // Bare, `fo?` is a glob that also matches `foo`.
    f.repo
        .run_jj(&["bookmark", "create", r#""fo?""#, "-r", "feat"]);
    let jj = f.repo.runner();
    jj.forget_bookmark("fo?").unwrap();
    jj.forget_bookmark(TRICKY).unwrap();
    let list = f
        .repo
        .run_jj(&["bookmark", "list", "-T", r#"name ++ "\n""#]);
    let names: Vec<&str> = list.lines().collect();
    assert!(names.contains(&"foo"), "{names:?}");
    assert!(!names.contains(&r#""fo?""#), "{names:?}");
    assert!(!names.contains(&r#""foo@origin""#), "{names:?}");
    assert_eq!(commit_of(&f.repo, "foo@origin"), f.remote_foo);
}

#[test]
fn push_sends_the_bookmark_of_that_name() {
    let Some(f) = fixture() else { return };
    f.repo.runner().push_bookmark(TRICKY, "origin").unwrap();
    let pushed = commit_of(&f.repo, r#""foo@origin"@origin"#);
    assert_eq!(pushed, f.local_tricky);
    assert_eq!(commit_of(&f.repo, "foo@origin"), f.remote_foo);
}

#[test]
fn undo_reads_and_moves_the_bookmark_of_that_name() {
    let Some(f) = fixture() else { return };
    let repo = JjRepo::new(f.repo.path().to_path_buf());
    let targets = repo.targets(TRICKY, "origin").unwrap();
    assert_eq!(targets.local.as_deref(), Some(f.local_tricky.as_str()));
    assert_eq!(targets.remote, None);

    let feat = commit_of(&f.repo, "feat");
    repo.set_bookmark(TRICKY, &feat).unwrap();
    assert_eq!(commit_of(&f.repo, &revset::symbol(TRICKY)), feat);

    repo.push_bookmark(TRICKY, "origin").unwrap();
    let targets = repo.targets(TRICKY, "origin").unwrap();
    assert_eq!(targets.remote.as_deref(), Some(feat.as_str()));

    repo.delete_bookmark(TRICKY).unwrap();
    assert_eq!(repo.targets(TRICKY, "origin").unwrap().local, None);
    assert_eq!(commit_of(&f.repo, "foo@origin"), f.remote_foo);
}

/// A repo whose trunk is the remote bookmark `rel@2`, named by `alias`.
fn trunk_named_rel_at_2(alias: &str) -> Option<JjTestRepo> {
    if !common::jj_available() {
        return None;
    }
    let repo = JjTestRepo::new();
    repo.commit_and_bookmark("rel.rs", "rel\n", "release", r#""rel@2""#);
    let mut push =
        jjpr::jj::version::push_new_bookmark_args(jjpr::jj::version::installed_jj_version())
            .to_vec();
    push.extend([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        r#"exact:"rel@2""#,
    ]);
    repo.run_jj(&push);
    repo.run_jj(&[
        "config",
        "set",
        "--repo",
        r#"revset-aliases."trunk()""#,
        alias,
    ]);
    Some(repo)
}

#[test]
fn a_trunk_name_that_is_not_a_plain_symbol_comes_back_unquoted() {
    for alias in [
        r#"remote_bookmarks(exact:"rel@2", exact:"origin")"#,
        // A TOML literal string: jj parses a config value as TOML first.
        r#"'"rel@2"@origin'"#,
        r#""'rel@2'@origin""#,
    ] {
        let Some(repo) = trunk_named_rel_at_2(alias) else {
            return;
        };
        let jj = repo.runner();
        let name = jj.get_default_branch().unwrap();
        assert_eq!(name, "rel@2", "trunk() = {alias}");
        assert_eq!(
            commit_of(&repo, &revset::symbol(&name)),
            commit_of(&repo, "trunk()"),
            "the name resolves as a revset: trunk() = {alias}"
        );
    }
}

#[test]
fn names_with_quotes_backslashes_and_control_characters_round_trip() {
    let Some(f) = fixture() else { return };
    let repo = JjRepo::new(f.repo.path().to_path_buf());
    let feat = commit_of(&f.repo, "feat");
    for name in ["say\"hi\"", "back\\slash", "tab\there"] {
        repo.set_bookmark(name, &feat).unwrap();
        let targets = repo.targets(name, "origin").unwrap();
        assert_eq!(targets.local.as_deref(), Some(feat.as_str()), "{name:?}");
        assert_eq!(
            commit_of(&f.repo, &revset::symbol(name)),
            feat,
            "{name:?} as a revset"
        );
        f.repo.runner().forget_bookmark(name).unwrap();
        assert_eq!(
            repo.targets(name, "origin").unwrap().local,
            None,
            "{name:?}"
        );
    }
}
