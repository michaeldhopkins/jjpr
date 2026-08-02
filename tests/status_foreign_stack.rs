//! Real-jj proof that `status` discovers the whole stack down to trunk
//! regardless of author (`build_status_graph`), while the mutating commands
//! stay scoped to your own bookmarks (`build_change_graph` / `mine()`).
//!
//! Mirrors the case that motivated the redesign: an empty working copy sitting
//! on a coworker-authored bookmark. `status` must show that branch; submit /
//! watch / merge must not (they only ever act on your own bookmarks).
//!
//! Gated on `jj` being installed.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use jjpr::graph::change_graph::{build_change_graph, build_status_graph, ChangeGraph};
use jjpr::jj::JjRunner;
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
        // Guarantee a defined user.email so a bare CI (no global config) can
        // author commits. On a dev box with global config, global wins — but
        // it stays consistent between these calls and JjRunner, which is all
        // `mine()` needs (author-at-creation == user.email-at-query).
        repo.run(&["config", "set", "--repo", "user.name", "Me"]);
        repo.run(&["config", "set", "--repo", "user.email", "me@example.invalid"]);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Run a jj command WITHOUT forcing a user config, so authorship resolves
    /// the same way JjRunner resolves it (config precedence, no `--config`).
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

fn bookmark_names(graph: &ChangeGraph) -> HashSet<String> {
    graph
        .stacks
        .iter()
        .flat_map(|s| s.segments.iter())
        .flat_map(|seg| seg.bookmarks.iter())
        .map(|b| b.name.clone())
        .collect()
}

#[test]
fn status_shows_foreign_stack_that_mine_scoping_hides() {
    if !jj_available() {
        return;
    }
    let repo = Repo::new();

    // My own stack (authored by the resolved user.email → in `mine()`).
    repo.run(&["new", "root()", "-m", "mine work"]);
    repo.run(&["bookmark", "set", "mine-feat", "-r", "@"]);

    // A coworker's stack, authored by a distinctly different email so it is
    // NOT in `mine()`. The per-command `--config` overrides config precedence.
    repo.run(&[
        "new",
        "root()",
        "--config=user.email=coworker@example.invalid",
        "-m",
        "coworker work",
    ]);
    repo.run(&["bookmark", "set", "coworker-feat", "-r", "@"]);

    // Sit directly on the coworker's branch with an empty, unbookmarked working
    // copy — exactly the mbc situation.
    repo.run(&["new", "coworker-feat", "-m", "my empty top"]);

    let runner = JjRunner::new(repo.path().to_path_buf()).expect("runner");

    // Mutating-command discovery: only my bookmark, never the coworker's.
    let mine = bookmark_names(&build_change_graph(&runner).expect("change graph"));
    assert!(mine.contains("mine-feat"), "mine() graph should include my bookmark: {mine:?}");
    assert!(
        !mine.contains("coworker-feat"),
        "mine() graph must NOT include a coworker's bookmark: {mine:?}"
    );

    // Broad status discovery (positional / --all): all your stacks plus the
    // coworker branch you're on — author-agnostic, both show up.
    let broad = bookmark_names(&build_status_graph(&runner, true).expect("broad status graph"));
    assert!(broad.contains("mine-feat"), "broad status should include my other stack: {broad:?}");
    assert!(
        broad.contains("coworker-feat"),
        "broad status must include the coworker branch I'm stacked on: {broad:?}"
    );

    // Bare working-copy view (`::@`): the coworker branch I'm sitting on, but
    // NOT my unrelated sibling stack (which the working copy doesn't reach).
    let bare = bookmark_names(&build_status_graph(&runner, false).expect("bare status graph"));
    assert!(
        bare.contains("coworker-feat"),
        "bare status must include the branch under the working copy: {bare:?}"
    );
    assert!(
        !bare.contains("mine-feat"),
        "bare status must not reach an unrelated sibling stack: {bare:?}"
    );
}
