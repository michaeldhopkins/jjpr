mod common;

use jjpr::graph::change_graph;
use jjpr::jj::Jj;
use jjpr::submit::analyze;

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
    let entries = jj.get_changes_to_commit(&bookmarks[0].commit_id).unwrap();

    assert!(!entries.is_empty());
    let entry = &entries[0];
    assert_eq!(entry.commit_id, bookmarks[0].commit_id);
    assert_eq!(entry.change_id, bookmarks[0].change_id);
    assert_eq!(entry.author_name, "Test User");
    assert_eq!(entry.author_email, "test@jjpr.dev");
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

#[test]
fn test_infer_bookmark_from_working_copy() {
    if !common::jj_available() {
        return;
    }

    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("auth.rs", "// auth\n", "Add authentication", "auth");
    repo.commit_and_bookmark("profile.rs", "// profile\n", "Add profile", "profile");

    let jj = repo.runner();
    let graph = change_graph::build_change_graph(&jj).unwrap();

    // Working copy is at @, which is the child of the "profile" commit.
    // The stack contains auth -> profile, so inference should return "profile".
    let inferred = analyze::infer_target_bookmark(&graph, &jj).unwrap();
    assert_eq!(inferred.as_deref(), Some("profile"));
}

#[test]
fn test_push_after_squash() {
    if !common::jj_available() {
        return;
    }

    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("feature.rs", "// v1\n", "Add feature", "feature");

    let jj = repo.runner();

    // First push
    jj.push_bookmark("feature", "origin").unwrap();

    // Amend via squash: write new content in working copy, then squash into feature
    repo.write_file("feature.rs", "// v2 amended\n");
    repo.run_jj(&["squash", "--into", "feature"]);

    // Second push should succeed (jj force-pushes diverged bookmarks by design)
    jj.push_bookmark("feature", "origin").unwrap();
}

/// `is_conflicted` must answer "does ANY commit in this revset conflict",
/// including for a multi-commit revset.
///
/// The previous implementation templated `if(conflict, ...)` per commit and
/// compared the whole output to `"true"`. For two commits that yields
/// `"falsetrue"`, which compares unequal — so a range containing a conflict
/// reported *clean*. The merge reconcile screens a segment's whole commit range
/// before pushing, so that silent false-negative would have let jjpr try to push
/// a conflicted ancestor and fall back to jj's bare "Won't push commit <sha>".
#[test]
fn is_conflicted_detects_a_conflict_anywhere_in_a_range() {
    if !common::jj_available() {
        return;
    }
    let repo = common::JjTestRepo::new();
    let short = |r: &str| {
        repo.run_jj(&["--ignore-working-copy", "log", "-r", r, "--no-graph", "-T", "change_id.short(8)"])
            .trim()
            .to_string()
    };

    repo.write_file("f.txt", "l1\nl2\n");
    repo.run_jj(&["describe", "-m", "BASE"]);
    repo.run_jj(&["bookmark", "create", "master", "-r", "@"]);

    // A conflicts once restacked; B, the bookmark tip, resolves it — so the tip
    // reads clean while its ancestor stays conflicted.
    repo.run_jj(&["new", "-m", "A"]);
    repo.write_file("f.txt", "OURS\nl2\n");
    repo.run_jj(&["status"]);
    let a = short("@");
    repo.run_jj(&["new", "-m", "B"]);
    repo.write_file("g.txt", "g\n");
    repo.run_jj(&["status"]);
    repo.run_jj(&["bookmark", "create", "feat", "-r", "@"]);

    repo.run_jj(&["new", "master", "-m", "M"]);
    repo.write_file("f.txt", "THEIRS\nl2\n");
    repo.run_jj(&["status"]);
    repo.run_jj(&["bookmark", "set", "master", "-r", "@"]);
    repo.run_jj(&["rebase", "-s", &a, "-d", "master"]);
    repo.run_jj(&["edit", "feat"]);
    repo.write_file("f.txt", "RESOLVED\nl2\n");
    repo.run_jj(&["status"]);

    let jj = repo.runner();
    assert!(
        !jj.is_conflicted("feat").unwrap(),
        "the tip resolves the conflict, so the tip alone looks clean"
    );
    assert!(
        jj.is_conflicted(&format!("{a}::feat")).unwrap(),
        "but the range still contains a conflicted ancestor"
    );
}

/// The conflict screen addresses a segment by its rebase *root*, a change ID.
/// When that change is divergent, jj refuses to resolve it as a bare symbol
/// ("Error: Change ID <x> is divergent"), so `<root>::<bookmark>` fails
/// outright. Wrapping the root in `change_id()` makes it a function call rather
/// than a symbol, which resolves to both copies; intersecting with the
/// bookmark's ancestry then selects the copy actually in the segment.
///
/// Reached only when a racing process diverges the stack *during* reconcile —
/// divergence already present is caught by the repo-wide gate that runs first.
///
/// This pins jj's behaviour, which is the whole reason for the wrapper.
#[test]
fn a_divergent_rebase_root_is_only_screenable_via_change_id() {
    if !common::jj_available() {
        return;
    }
    let repo = common::JjTestRepo::new();
    let short = |r: &str| {
        repo.run_jj(&["--ignore-working-copy", "log", "-r", r, "--no-graph", "-T", "change_id.short(8)"])
            .trim()
            .to_string()
    };

    repo.write_file("f.txt", "l1\n");
    repo.run_jj(&["describe", "-m", "A"]);
    let a = short("@");
    let good_op = repo
        .run_jj(&["op", "log", "--no-graph", "-T", "id.short() ++ \"\\n\""])
        .lines()
        .next()
        .expect("an operation")
        .trim()
        .to_string();

    // Two concurrent rewrites of A reconcile to a divergent change: both kept.
    repo.write_file("f.txt", "OURS\n");
    repo.run_jj(&["status"]);
    repo.run_jj(&["--at-operation", &good_op, "describe", "-m", "A (theirs)"]);
    repo.run_jj(&["status"]);

    // Build a segment on one copy, so the divergent change is its rebase root.
    repo.run_jj(&["new", &format!("{a}/0"), "-m", "B"]);
    repo.run_jj(&["bookmark", "create", "feat", "-r", "@"]);

    let jj = repo.runner();
    assert!(
        jj.is_conflicted(&format!("{a}::feat")).is_err(),
        "a bare divergent change ID must not resolve — if this ever starts \
         succeeding, jj changed and the change_id() wrapper can be revisited"
    );
    assert!(
        jj.is_conflicted(&format!("change_id({a})::feat")).is_ok(),
        "the change_id() form must resolve despite the divergence"
    );
}

/// End-to-end over REAL jj: a divergent change with both copies in one ancestry
/// chain must yield two DISTINCT segments, and submit's detector must see it.
///
/// Every other test of this behaviour uses stubs, which means they only prove the
/// logic is self-consistent. This proves the shape is reachable in real jj at all
/// (it is — rebase one copy of a divergent change onto the other), that jjpr's
/// bookmark/log templates carry it through, and that the graph keyed by commit id
/// keeps the copies apart where change-id keying merged them.
#[test]
fn real_divergent_change_in_one_chain_stays_two_segments() {
    if !common::jj_available() {
        return;
    }
    let repo = common::JjTestRepo::new();
    let one = |r: &str, t: &str| {
        repo.run_jj(&["--ignore-working-copy", "log", "-r", r, "--no-graph", "-T", t])
            .trim()
            .to_string()
    };

    repo.write_file("base.txt", "base\n");
    repo.run_jj(&["describe", "-m", "base"]);
    repo.run_jj(&["bookmark", "create", "master", "-r", "@"]);
    repo.run_jj(&["new", "-m", "layer"]);

    // Two concurrent rewrites of the same change reconcile to a divergent change.
    // The copies must differ in CONTENT: with identical diffs, rebasing one onto
    // the other produces an empty commit and jj drops it, so the shape under test
    // never forms. The resulting conflict is incidental — jjpr's graph does not
    // care, and keeping it makes the fixture honest about what divergence is.
    repo.write_file("one.txt", "one\n");
    repo.run_jj(&["status"]);
    let good_op = repo
        .run_jj(&["op", "log", "--no-graph", "-T", "id.short() ++ \"\\n\""])
        .lines()
        .next()
        .expect("an operation")
        .trim()
        .to_string();
    repo.write_file("one.txt", "ours\n");
    repo.run_jj(&["status"]);
    repo.run_jj(&["--at-operation", &good_op, "describe", "-m", "layer (theirs)"]);
    repo.run_jj(&["status"]);

    let divergent = repo.run_jj(&[
        "--ignore-working-copy", "log", "-r", "divergent()", "--no-graph",
        "-T", "commit_id.short() ++ \"\\n\"",
    ]);
    let copies: Vec<&str> = divergent.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    assert_eq!(copies.len(), 2, "expected one divergent change on two commits: {copies:?}");

    // Put one copy ON TOP of the other, so both sit in a single chain.
    repo.run_jj(&["rebase", "-r", copies[0], "-d", copies[1]]);
    // The rebase gave the moved copy a new commit id; find it as the divergent
    // descendant of the one that stayed put.
    let upper = one(
        &format!("divergent() & (descendants({c}) ~ {c})", c = copies[1]),
        "commit_id.short() ++ \"\\n\"",
    );
    let upper = upper
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .expect("the rebased copy must exist")
        .to_string();

    repo.run_jj(&["bookmark", "create", "lower", "-r", copies[1]]);
    repo.run_jj(&["bookmark", "create", "upper", "-r", &upper]);
    repo.run_jj(&["new", "upper"]);

    let graph = jjpr::graph::change_graph::build_change_graph(&repo.runner()).unwrap();

    let seg_of = |name: &str| {
        graph.stacks.iter().flat_map(|s| s.segments.iter()).position(|seg| {
            seg.bookmarks.iter().any(|b| b.name == name)
        })
    };
    let (lo, up) = (seg_of("lower"), seg_of("upper"));
    assert!(lo.is_some() && up.is_some(), "both bookmarks must be segments: {lo:?} {up:?}");
    assert_ne!(lo, up, "the two copies are distinct commits and must be distinct segments");

    // And submit's detector must fire on the real segments, not just on stubs.
    let narrowed: Vec<jjpr::jj::types::NarrowedSegment> = graph
        .stacks
        .iter()
        .flat_map(|s| s.segments.iter())
        .filter_map(|seg| {
            Some(jjpr::jj::types::NarrowedSegment {
                bookmark: seg.bookmarks.first()?.clone(),
                changes: seg.changes.clone(),
                merge_source_names: seg.merge_source_names.clone(),
            })
        })
        .collect();
    let found = jjpr::submit::plan::divergent_changes_in_stack(&narrowed);
    assert_eq!(found.len(), 1, "submit must see exactly one divergent change: {found:?}");
    assert_eq!(found[0].commit_ids.len(), 2, "naming both commits: {:?}", found[0]);
}
