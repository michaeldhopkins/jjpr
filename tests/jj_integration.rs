mod common;

use stacker::graph::change_graph;
use stacker::jj::Jj;

#[test]
fn test_real_jj_bookmark_parsing() {
    if !common::jj_available() {
        return;
    }

    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("auth.rs", "// auth\n", "Add authentication", "auth");

    let jj = repo.runner();
    let bookmarks = jj.get_my_bookmarks().unwrap();

    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].name, "auth");
    assert!(!bookmarks[0].commit_id.is_empty());
    assert!(!bookmarks[0].change_id.is_empty());
    assert!(!bookmarks[0].has_remote);
    assert!(!bookmarks[0].is_synced);
}

#[test]
fn test_real_jj_log_parsing() {
    if !common::jj_available() {
        return;
    }

    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark(
        "auth.rs",
        "// auth\n",
        "Add authentication\n\nDetailed auth description",
        "auth",
    );

    let jj = repo.runner();
    let bookmarks = jj.get_my_bookmarks().unwrap();
    let entries = jj.get_branch_changes(&bookmarks[0].commit_id).unwrap();

    assert!(!entries.is_empty());
    let entry = &entries[0];
    assert_eq!(entry.commit_id, bookmarks[0].commit_id);
    assert_eq!(entry.change_id, bookmarks[0].change_id);
    assert_eq!(entry.author_name, "Test User");
    assert_eq!(entry.author_email, "test@stacker.dev");
    assert!(entry.description.starts_with("Add authentication"));
    assert_eq!(entry.description_first_line, "Add authentication");
    assert_eq!(entry.parents.len(), 1, "should have one parent (the initial commit)");
}

#[test]
fn test_real_jj_graph_linear_stack() {
    if !common::jj_available() {
        return;
    }

    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("auth.rs", "// auth\n", "Add authentication", "auth");
    repo.commit_and_bookmark("profile.rs", "// profile\n", "Add user profile", "profile");

    let jj = repo.runner();
    let graph = change_graph::build_change_graph(&jj).unwrap();

    assert_eq!(graph.bookmarks.len(), 2);
    assert!(graph.bookmarks.contains_key("auth"));
    assert!(graph.bookmarks.contains_key("profile"));
    assert_eq!(graph.excluded_bookmark_count, 0);

    assert_eq!(graph.stacks.len(), 1, "should form a single stack");
    let stack = &graph.stacks[0];
    assert_eq!(stack.segments.len(), 2);
    assert_eq!(stack.segments[0].bookmarks[0].name, "auth");
    assert_eq!(stack.segments[1].bookmarks[0].name, "profile");
}

#[test]
fn test_real_jj_default_branch() {
    if !common::jj_available() {
        return;
    }

    let repo = common::JjTestRepo::new();
    let jj = repo.runner();
    let default = jj.get_default_branch().unwrap();
    assert_eq!(default, "main");
}
