//! `jjpr undo` and `jjpr redo` against a real jj repo and a bare git remote.
//!
//! The forge is [`ModelForge`], an in-memory forge whose branches are the bare
//! remote's own refs, so a push jj makes is what the forge sees. Each test
//! drives submit the way `jjpr submit` does (through the recording wrappers),
//! then undoes and redoes it through `jjpr::undo::run`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use jjpr::forge::types::{
    ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequest, PullRequestRef,
    RepoInfo, ReviewSummary,
};
use jjpr::forge::{Forge, ForgeKind};
use jjpr::graph::change_graph;
use jjpr::jj::Jj;
use jjpr::submit::{analyze, execute, plan, resolve};
use jjpr::undo::journal::Action;
use jjpr::undo::{
    Context, Direction, JjRepo, Journal, Meta, Options, Recorder, RecordingForge, RecordingJj,
};

#[derive(Debug, Clone, Default)]
struct ModelPr {
    head: String,
    base: String,
    title: String,
    body: String,
    draft: bool,
    open: bool,
    merged_at: Option<String>,
    reviewers: Vec<String>,
}

#[derive(Default)]
struct Model {
    prs: BTreeMap<u64, ModelPr>,
    /// Comment id to its PR and body.
    comments: BTreeMap<u64, (u64, String)>,
    next: u64,
    /// A PR the forge closes once jjpr reads its state after a push, as GitHub
    /// closes one whose branch has nothing left to merge.
    close_on_read: Option<u64>,
    /// Forge writes made since the gate was last set, and the first that fails.
    writes: usize,
    fail_writes_from: Option<usize>,
    fail_write_at: Option<usize>,
    /// Closing lands, then answers with an error, as a timed-out request can.
    close_lands_then_fails: bool,
}

/// An in-memory forge over a bare git remote.
#[derive(Clone)]
struct ModelForge {
    model: Arc<Mutex<Model>>,
    origin: PathBuf,
}

impl ModelForge {
    fn new(origin: &Path) -> Self {
        Self {
            model: Arc::new(Mutex::new(Model {
                next: 1,
                ..Model::default()
            })),
            origin: origin.to_path_buf(),
        }
    }

    fn pr(&self, number: u64) -> ModelPr {
        self.model.lock().expect("model lock").prs[&number].clone()
    }

    fn pr_on(&self, head: &str) -> Option<u64> {
        let m = self.model.lock().expect("model lock");
        m.prs.iter().find(|(_, p)| p.head == head).map(|(n, _)| *n)
    }

    fn comments_on(&self, pr: u64) -> Vec<String> {
        let m = self.model.lock().expect("model lock");
        m.comments
            .values()
            .filter(|(p, _)| *p == pr)
            .map(|(_, b)| b.clone())
            .collect()
    }

    fn to_pr(number: u64, p: &ModelPr) -> PullRequest {
        let side = |r: &str| PullRequestRef {
            ref_name: r.to_string(),
            label: String::new(),
            sha: String::new(),
        };
        PullRequest {
            number,
            html_url: format!("https://example.test/pull/{number}"),
            title: p.title.clone(),
            body: Some(p.body.clone()),
            base: side(&p.base),
            head: side(&p.head),
            draft: p.draft,
            node_id: String::new(),
            merged_at: p.merged_at.clone(),
            requested_reviewers: p.reviewers.clone(),
            author: "me".to_string(),
            stack: None,
        }
    }

    /// Make the forge refuse every write from the `n`-th (counted from 0) on.
    fn fail_writes_from(&self, n: usize) {
        let mut m = self.model.lock().expect("model lock");
        m.writes = 0;
        m.fail_writes_from = Some(n);
    }

    fn gate(&self) -> Result<()> {
        let mut m = self.model.lock().expect("model lock");
        let n = m.writes;
        m.writes += 1;
        if m.fail_writes_from.is_some_and(|from| n >= from) || m.fail_write_at == Some(n) {
            anyhow::bail!("HTTP 503: write {n} refused");
        }
        Ok(())
    }

    fn with_pr<T>(&self, number: u64, f: impl FnOnce(&mut ModelPr) -> T) -> Result<T> {
        let mut m = self.model.lock().expect("model lock");
        let pr = m
            .prs
            .get_mut(&number)
            .ok_or_else(|| anyhow::anyhow!("HTTP 404: no PR {number}"))?;
        Ok(f(pr))
    }
}

impl Forge for ModelForge {
    fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
        let m = self.model.lock().expect("model lock");
        Ok(m.prs
            .iter()
            .filter(|(_, p)| p.open)
            .map(|(n, p)| Self::to_pr(*n, p))
            .collect())
    }
    fn create_pr(
        &self,
        _: &str,
        _: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        draft: bool,
    ) -> Result<PullRequest> {
        let mut m = self.model.lock().expect("model lock");
        let number = m.next;
        m.next += 1;
        let pr = ModelPr {
            head: head.into(),
            base: base.into(),
            title: title.into(),
            body: body.into(),
            draft,
            open: true,
            ..ModelPr::default()
        };
        let out = Self::to_pr(number, &pr);
        m.prs.insert(number, pr);
        Ok(out)
    }
    fn update_pr_base(&self, _: &str, _: &str, n: u64, base: &str) -> Result<()> {
        self.gate()?;
        self.with_pr(n, |p| p.base = base.into())
    }
    fn request_reviewers(&self, _: &str, _: &str, n: u64, who: &[String]) -> Result<()> {
        self.with_pr(n, |p| {
            for w in who {
                if !p.reviewers.contains(w) {
                    p.reviewers.push(w.clone());
                }
            }
        })
    }
    fn list_comments(&self, _: &str, _: &str, n: u64) -> Result<Vec<IssueComment>> {
        let m = self.model.lock().expect("model lock");
        Ok(m.comments
            .iter()
            .filter(|(_, (p, _))| *p == n)
            .map(|(id, (_, b))| IssueComment {
                id: *id,
                body: Some(b.clone()),
            })
            .collect())
    }
    fn create_comment(&self, _: &str, _: &str, n: u64, body: &str) -> Result<IssueComment> {
        self.gate()?;
        let mut m = self.model.lock().expect("model lock");
        let id = 1000 + m.next;
        m.next += 1;
        m.comments.insert(id, (n, body.into()));
        Ok(IssueComment {
            id,
            body: Some(body.into()),
        })
    }
    fn update_comment(&self, _: &str, _: &str, id: u64, body: &str) -> Result<()> {
        self.gate()?;
        let mut m = self.model.lock().expect("model lock");
        let c = m
            .comments
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("HTTP 404"))?;
        c.1 = body.into();
        Ok(())
    }
    fn delete_comment(&self, _: &str, _: &str, id: u64) -> Result<()> {
        self.gate()?;
        self.model.lock().expect("model lock").comments.remove(&id);
        Ok(())
    }
    fn update_pr_body(&self, _: &str, _: &str, n: u64, body: &str) -> Result<()> {
        self.with_pr(n, |p| p.body = body.into())
    }
    fn mark_pr_ready(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.with_pr(n, |p| p.draft = false)
    }
    fn get_authenticated_user(&self) -> Result<String> {
        Ok("me".into())
    }
    fn find_merged_pr(&self, _: &str, _: &str, head: &str) -> Result<Option<PullRequest>> {
        let m = self.model.lock().expect("model lock");
        Ok(m.prs
            .iter()
            .find(|(_, p)| p.merged_at.is_some() && p.head == head)
            .map(|(n, p)| Self::to_pr(*n, p)))
    }
    fn merge_pr(&self, _: &str, _: &str, n: u64, _: MergeMethod) -> Result<()> {
        self.with_pr(n, |p| {
            p.merged_at = Some("2026-10-06T00:00:00Z".into());
            p.open = false;
        })
    }
    fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
        Ok(ChecksStatus::Pass)
    }
    fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
        Ok(ReviewSummary {
            approved_count: 0,
            changes_requested: false,
        })
    }
    fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
        unimplemented!()
    }
    fn get_pr_state(&self, _: &str, _: &str, n: u64) -> Result<PrState> {
        if self.model.lock().expect("model lock").close_on_read.take() == Some(n) {
            self.with_pr(n, |p| p.open = false)?;
        }
        Ok(self.get_pr("", "", n)?.1)
    }
    fn get_pr(&self, _: &str, _: &str, n: u64) -> Result<(PullRequest, PrState)> {
        let p = self.with_pr(n, |p| p.clone())?;
        let state = if p.open { "open" } else { "closed" };
        let state = PrState {
            merged: p.merged_at.is_some(),
            state: state.into(),
        };
        Ok((Self::to_pr(n, &p), state))
    }
    fn close_pr(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.gate()?;
        self.with_pr(n, |p| p.open = false)?;
        if self
            .model
            .lock()
            .expect("model lock")
            .close_lands_then_fails
        {
            anyhow::bail!("HTTP 504: gateway timeout");
        }
        Ok(())
    }
    fn reopen_pr(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.gate()?;
        self.with_pr(n, |p| p.open = true)
    }
    fn convert_to_draft(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.with_pr(n, |p| p.draft = true)
    }
    fn remove_reviewers(&self, _: &str, _: &str, n: u64, who: &[String]) -> Result<()> {
        self.with_pr(n, |p| p.reviewers.retain(|r| !who.contains(r)))
    }
    fn get_branch_head(&self, _: &str, _: &str, branch: &str) -> Result<Option<String>> {
        Ok(remote_head(&self.origin, branch))
    }
}

fn remote_head(origin: &Path, branch: &str) -> Option<String> {
    let out = Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(origin)
        .output()
        .expect("test fixture");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn meta(command: &str) -> Meta {
    Meta {
        command: command.into(),
        remote: "origin".into(),
        forge: ForgeKind::GitHub,
        owner: "o".into(),
        repo: "r".into(),
    }
}

fn recorder(repo: &common::JjTestRepo, command: &str) -> Arc<Recorder> {
    let path = repo.path().to_path_buf();
    Recorder::start(
        Journal::for_repo(&path).expect("test fixture"),
        Box::new(JjRepo::new(path)),
        meta(command),
    )
}

/// `jjpr submit <target>` as main.rs runs it, recorded for undo.
fn submit(repo: &common::JjTestRepo, forge: &ModelForge, target: &str, reviewers: &[String]) {
    let rec = recorder(repo, "submit");
    let jj = RecordingJj::new(repo.runner(), rec.clone());
    let recording = RecordingForge::new(Box::new(forge.clone()), rec);
    let graph = change_graph::build_change_graph(&jj).expect("test fixture");
    let analysis = analyze::analyze_submission_graph(&graph, target).expect("test fixture");
    let segments = resolve::resolve_bookmark_selections(&analysis.relevant_segments, false)
        .expect("test fixture");
    let plan = plan::create_submission_plan(
        &recording,
        &segments,
        "origin",
        &RepoInfo {
            owner: "o".into(),
            repo: "r".into(),
        },
        ForgeKind::GitHub,
        "main",
        &plan::SubmitOptions {
            draft_mode: plan::DraftMode::Default,
            reviewers,
            reviewer_scope: Default::default(),
            stack_base: None,
            stack_nav: Default::default(),
            dry_run: false,
        },
    )
    .expect("test fixture");
    execute::execute_submission_plan(&jj, &recording, &plan).expect("test fixture");
}

struct Run {
    result: Result<()>,
    out: String,
}

fn act(
    repo: &common::JjTestRepo,
    forge: &ModelForge,
    direction: Direction,
    force: bool,
    dry_run: bool,
) -> Run {
    let path = repo.path().to_path_buf();
    let journal = Journal::for_repo(&path).expect("test fixture");
    let jj_repo = JjRepo::new(path);
    let forge_for =
        |_: &jjpr::undo::journal::Entry| -> Result<Box<dyn Forge>> { Ok(Box::new(forge.clone())) };
    let cx = Context {
        journal: &journal,
        repo: &jj_repo,
        forge_for: &forge_for,
        watch_running: false,
        now: 0,
    };
    let mut out = Vec::new();
    let result = jjpr::undo::run(
        &cx,
        Options {
            direction,
            force,
            dry_run,
        },
        &mut out,
    );
    Run {
        result,
        out: String::from_utf8(out).expect("test fixture"),
    }
}

fn undo(repo: &common::JjTestRepo, forge: &ModelForge, force: bool) -> Run {
    act(repo, forge, Direction::Undo, force, false)
}

fn redo(repo: &common::JjTestRepo, forge: &ModelForge) -> Run {
    act(repo, forge, Direction::Redo, false, false)
}

fn ok(run: Run) -> String {
    assert!(run.result.is_ok(), "{:?}\n{}", run.result, run.out);
    run.out
}

fn err(run: Run) -> String {
    format!("{:#}", run.result.expect_err(&run.out))
}

fn local(repo: &common::JjTestRepo, bookmark: &str) -> String {
    repo.run_jj(&["log", "--no-graph", "-r", bookmark, "-T", "commit_id"])
}

fn head_op(repo: &common::JjTestRepo) -> String {
    repo.run_jj(&[
        "op",
        "log",
        "-n1",
        "--no-graph",
        "-T",
        "id",
        "--ignore-working-copy",
    ])
}

fn two_bookmarks() -> common::JjTestRepo {
    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("a.rs", "// a\n", "Add a", "a");
    repo.commit_and_bookmark("b.rs", "// b\n", "Add b", "b");
    repo
}

fn entries(repo: &common::JjTestRepo) -> Vec<jjpr::undo::journal::Entry> {
    Journal::for_repo(repo.path())
        .expect("test fixture")
        .load()
        .expect("test fixture")
        .entries
}

#[test]
fn a_first_submit_needs_force_and_without_it_nothing_changes() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let (pa, pb) = (forge.pr_on("a").unwrap(), forge.pr_on("b").unwrap());
    let comments = (forge.comments_on(pa), forge.comments_on(pb));
    assert!(!comments.0.is_empty(), "submit wrote stack comments");
    let op = head_op(&repo);

    let message = err(undo(&repo, &forge, false));
    assert!(
        message.contains("can't undo all of `jjpr submit`"),
        "{message}"
    );
    assert!(
        message.contains("without --force, so it changed nothing:"),
        "{message}"
    );
    assert!(
        message.contains("#1, which the submit opened, would be closed"),
        "{message}"
    );
    assert!(
        message.contains("Rerun with --force to undo all of it: jjpr undo --force"),
        "{message}"
    );
    assert_eq!(head_op(&repo), op, "not even the local repo moved");
    assert_eq!((forge.comments_on(pa), forge.comments_on(pb)), comments);
    assert!(forge.pr(pa).open && forge.pr(pb).open);
    assert!(remote_head(repo.origin_path(), "a").is_some());
    assert_eq!(entries(&repo)[0].state, jjpr::undo::journal::State::Done);

    let out = ok(undo(&repo, &forge, true));
    assert!(out.contains("Close #1, which the submit opened"), "{out}");
    assert!(out.contains("Delete branch 'a' from origin"), "{out}");
    assert!(!forge.pr(pa).open && !forge.pr(pb).open);
    assert!(forge.comments_on(pa).is_empty() && forge.comments_on(pb).is_empty());
    assert_eq!(remote_head(repo.origin_path(), "a"), None);
    assert_eq!(remote_head(repo.origin_path(), "b"), None);
    assert!(!local(&repo, "a").is_empty(), "the local bookmark stays");

    let out = ok(redo(&repo, &forge));
    assert!(out.contains("Reopen #1"), "{out}");
    assert!(forge.pr(pa).open && forge.pr(pb).open);
    assert_eq!(
        remote_head(repo.origin_path(), "a").as_deref(),
        Some(local(&repo, "a").as_str())
    );
    assert!(!forge.comments_on(pa).is_empty(), "comments posted again");
}

/// A dry run shows what would go through and every blocker.
#[test]
fn a_dry_run_lists_the_steps_and_what_blocks_them() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let out = ok(act(&repo, &forge, Direction::Undo, false, true));
    assert!(out.contains("Would undo `jjpr submit`"), "{out}");
    assert!(out.contains("Close #2, which the submit opened"), "{out}");
    assert!(
        out.contains("so the real run would change nothing:"),
        "{out}"
    );
    assert!(out.contains("Nothing was changed."), "{out}");
    let forced = ok(act(&repo, &forge, Direction::Undo, true, true));
    assert!(!forced.contains("would change nothing"), "{forced}");
}

/// A forge error partway through puts back every step already taken.
#[test]
fn a_failure_partway_puts_back_what_was_done() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let comments = forge.comments_on(1);
    let (a, b) = (
        remote_head(repo.origin_path(), "a"),
        remote_head(repo.origin_path(), "b"),
    );
    let local_a = local(&repo, "a");
    // Deleting both stack comments goes through; closing the first PR does not.
    {
        let mut m = forge.model.lock().unwrap();
        m.writes = 0;
        m.fail_write_at = Some(2);
    }
    let run = undo(&repo, &forge, true);
    let message = err(Run {
        result: run.result,
        out: run.out.clone(),
    });
    assert!(message.contains("The undo of `jjpr submit`"), "{message}");
    assert!(message.contains("HTTP 503"), "{message}");
    assert!(
        message.contains("jjpr put back everything it had changed, so nothing is changed"),
        "{message}"
    );
    assert!(
        run.out
            .contains("That step failed. Putting back what this undo changed:"),
        "{}",
        run.out
    );
    assert_eq!(forge.comments_on(1), comments, "the stack comment is back");
    assert!(forge.pr(1).open && forge.pr(2).open);
    assert_eq!(remote_head(repo.origin_path(), "a"), a);
    assert_eq!(remote_head(repo.origin_path(), "b"), b);
    assert_eq!(local(&repo, "a"), local_a);
    assert_eq!(entries(&repo)[0].state, jjpr::undo::journal::State::Done);

    forge.model.lock().unwrap().fail_write_at = None;
    ok(undo(&repo, &forge, true));
    assert!(!forge.pr(1).open);
}

/// When putting back fails too, the entry says it is partly undone, and
/// either command takes it from there.
#[test]
fn a_put_back_that_fails_leaves_a_partial_undo_either_command_resolves() {
    if !common::jj_available() {
        return;
    }
    for finish in [true, false] {
        let repo = two_bookmarks();
        let forge = ModelForge::new(repo.origin_path());
        submit(&repo, &forge, "b", &[]);
        let comments = forge.comments_on(1);
        forge.fail_writes_from(2);
        let message = err(undo(&repo, &forge, true));
        assert!(
            message.contains("Putting back what it had changed failed too"),
            "{message}"
        );
        assert!(
            message.contains(
                "It is partly undone. Fix the problem, then run `jjpr undo` to finish undoing \
                 it, or `jjpr redo` to put back what it did."
            ),
            "{message}"
        );
        assert_eq!(
            entries(&repo)[0].state,
            jjpr::undo::journal::State::PartlyUndone
        );
        forge.model.lock().unwrap().fail_writes_from = None;
        if finish {
            ok(undo(&repo, &forge, true));
            assert!(!forge.pr(1).open && !forge.pr(2).open);
            assert_eq!(remote_head(repo.origin_path(), "a"), None);
        } else {
            ok(redo(&repo, &forge));
            assert!(forge.pr(1).open && forge.pr(2).open);
            assert_eq!(forge.comments_on(1), comments);
        }
    }
}

/// Undo of a second submit, after an amend: the remote goes back to what the
/// first submit pushed, while the local amend stays.
#[test]
fn an_amended_resubmit_is_undone_on_the_remote_and_redone() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let (first_a, first_b) = (local(&repo, "a"), local(&repo, "b"));
    repo.run_jj(&["describe", "a", "-m", "Add a, amended"]);
    let (amended_a, amended_b) = (local(&repo, "a"), local(&repo, "b"));
    submit(&repo, &forge, "b", &[]);
    assert_eq!(
        remote_head(repo.origin_path(), "a"),
        Some(amended_a.clone())
    );

    let out = ok(undo(&repo, &forge, false));
    assert!(out.contains("Force-push 'b' back to"), "{out}");
    assert_eq!(remote_head(repo.origin_path(), "a"), Some(first_a));
    assert_eq!(remote_head(repo.origin_path(), "b"), Some(first_b));
    assert_eq!(local(&repo, "a"), amended_a, "the amend stays local");
    assert_eq!(local(&repo, "b"), amended_b);
    let divergent = repo.run_jj(&[
        "log",
        "--no-graph",
        "-r",
        "all()",
        "-T",
        "change_id ++ \"\\n\"",
    ]);
    let mut ids: Vec<&str> = divergent.lines().collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), before, "undo left no divergent change");

    let out = ok(redo(&repo, &forge));
    assert!(out.contains("again"), "{out}");
    assert_eq!(remote_head(repo.origin_path(), "a"), Some(amended_a));
    assert_eq!(remote_head(repo.origin_path(), "b"), Some(amended_b));
}

/// Two submits with nothing in between: undo takes back both, newest first,
/// and redo puts back both, oldest first.
#[test]
fn undo_goes_back_two_commands() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "a", &[]);
    submit(&repo, &forge, "b", &[]);
    assert_eq!(entries(&repo).len(), 2);
    ok(undo(&repo, &forge, true));
    assert_eq!(remote_head(repo.origin_path(), "b"), None);
    assert!(!forge.pr(2).open && forge.pr(1).open);
    ok(undo(&repo, &forge, true));
    assert_eq!(remote_head(repo.origin_path(), "a"), None);
    assert!(!forge.pr(1).open);
    assert_eq!(ok(undo(&repo, &forge, true)).trim(), "Nothing to undo.");
    ok(redo(&repo, &forge));
    assert!(forge.pr(1).open && !forge.pr(2).open);
    ok(redo(&repo, &forge));
    assert!(forge.pr(2).open);
    assert_eq!(
        remote_head(repo.origin_path(), "b").as_deref(),
        Some(local(&repo, "b").as_str())
    );
    assert_eq!(ok(redo(&repo, &forge)).trim(), "Nothing to redo.");
}

/// An amend between two submits means undoing the second cannot bring back
/// what the first left, so the first is dropped from the journal.
#[test]
fn an_edit_between_commands_ends_the_history_before_it() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    repo.run_jj(&["describe", "a", "-m", "Add a, amended"]);
    submit(&repo, &forge, "b", &[]);
    assert_eq!(entries(&repo).len(), 1);
    ok(undo(&repo, &forge, false));
    assert_eq!(ok(undo(&repo, &forge, false)).trim(), "Nothing to undo.");
}

fn run(dir: &Path, program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("test fixture");
    assert!(out.status.success(), "{program} {args:?}: {out:?}");
}

/// Descriptions of the commits `top`'s PR would carry, newest first.
fn top_pr_commits(repo: &common::JjTestRepo) -> Vec<String> {
    repo.run_jj(&[
        "log",
        "--no-graph",
        "-r",
        "trunk()..top",
        "-T",
        "description.first_line() ++ \"\\n\"",
    ])
    .lines()
    .map(str::to_string)
    .collect()
}

/// The case `jj undo` handles worst: a submit that restacked (a rebase, an
/// abandon and a push, each its own operation) after the bottom PR was
/// squash-merged and its branch deleted. One `jjpr undo` puts the stack back.
#[test]
fn a_restack_is_undone_whole() {
    if !common::jj_available() {
        return;
    }
    let repo = common::JjTestRepo::new();
    repo.commit_and_bookmark("bottom.rs", "// bottom\n", "Add bottom", "bottom");
    repo.commit_and_bookmark("top.rs", "// top\n", "Add top", "top");
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "top", &[]);
    let old_top = local(&repo, "top");

    // The forge squash-merges the bottom PR and deletes its branch.
    let side = tempfile::TempDir::new().unwrap();
    let origin = repo.origin_path().to_str().unwrap().to_string();
    run(side.path(), "jj", &["git", "clone", &origin, "clone"]);
    let clone = side.path().join("clone");
    run(
        &clone,
        "jj",
        &["config", "set", "--repo", "user.email", "forge@jjpr.dev"],
    );
    run(&clone, "jj", &["new", "main"]);
    std::fs::write(clone.join("bottom.rs"), "// bottom\n").unwrap();
    run(&clone, "jj", &["commit", "-m", "Add bottom (#1)"]);
    run(&clone, "jj", &["bookmark", "set", "main", "-r", "@-"]);
    run(&clone, "jj", &["git", "push", "--bookmark", "main"]);
    run(repo.origin_path(), "git", &["branch", "-D", "bottom"]);
    forge.merge_pr("o", "r", 1, MergeMethod::Squash).unwrap();

    // `jjpr submit`: snapshot and fetch, then everything recorded.
    let mut runner = repo.runner();
    runner.set_fetch_remote(Some("origin".into()));
    let before_fetch = runner.get_my_bookmarks().unwrap();
    runner.git_fetch().unwrap();
    {
        let rec = recorder(&repo, "submit");
        let jj = RecordingJj::new(runner, rec.clone());
        let recording = RecordingForge::new(Box::new(forge.clone()), rec);
        let segments = |jj: &dyn Jj| {
            let graph = change_graph::build_change_graph(jj).unwrap();
            let analysis = analyze::analyze_submission_graph(&graph, "top").unwrap();
            resolve::resolve_bookmark_selections(&analysis.relevant_segments, false).unwrap()
        };
        let make_plan = |segs: &[jjpr::jj::types::NarrowedSegment]| {
            plan::create_submission_plan(
                &recording,
                segs,
                "origin",
                &RepoInfo {
                    owner: "o".into(),
                    repo: "r".into(),
                },
                ForgeKind::GitHub,
                "main",
                &plan::SubmitOptions {
                    draft_mode: plan::DraftMode::Default,
                    reviewers: &[],
                    reviewer_scope: Default::default(),
                    stack_base: None,
                    stack_nav: Default::default(),
                    dry_run: false,
                },
            )
            .unwrap()
        };
        let segs = segments(&jj);
        let first = make_plan(&segs);
        let restacked = jjpr::submit::restack::restack_merged_base(
            &jj,
            &recording,
            &first,
            &segs,
            &before_fetch,
            false,
        )
        .unwrap();
        assert!(restacked);
        let segs = segments(&jj);
        execute::execute_submission_plan(&jj, &recording, &make_plan(&segs)).unwrap();
    }
    assert_eq!(top_pr_commits(&repo), vec!["Add top"]);
    assert_eq!(forge.pr(2).base, "main");
    let new_top = local(&repo, "top");

    let message = err(undo(&repo, &forge, false));
    assert!(
        message.contains("#2 can't go back to base 'bottom': origin no longer has that branch"),
        "{message}"
    );
    let out = ok(undo(&repo, &forge, true));
    assert!(out.contains("Restore the local repo to operation"), "{out}");
    assert!(out.contains("leaving its base as it is"), "{out}");
    assert_eq!(top_pr_commits(&repo), vec!["Add top", "Add bottom"]);
    assert_eq!(local(&repo, "top"), old_top);
    assert_eq!(remote_head(repo.origin_path(), "top"), Some(old_top));

    ok(redo(&repo, &forge));
    assert_eq!(top_pr_commits(&repo), vec!["Add top"]);
    assert_eq!(remote_head(repo.origin_path(), "top"), Some(new_top));
}

#[test]
fn a_change_to_the_repo_since_blocks_undo_and_changes_nothing() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    repo.run_jj(&["describe", "b", "-m", "Add b, later"]);
    let op = head_op(&repo);
    let message = err(undo(&repo, &forge, true));
    assert!(
        message.contains("the repo changed since, and that work would be lost"),
        "{message}"
    );
    assert!(message.contains("describe commit"), "{message}");
    assert_eq!(head_op(&repo), op, "nothing was changed");
    assert!(forge.pr(1).open);
}

#[test]
fn uncommitted_edits_block_undo_and_stay_on_disk() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    repo.write_file("wip.txt", "precious\n");
    let message = err(undo(&repo, &forge, false));
    assert!(
        message.contains("the repo changed since, and that work would be lost"),
        "{message}"
    );
    assert!(repo.path().join("wip.txt").exists());
}

#[test]
fn a_branch_someone_else_pushed_blocks_undo_even_with_force() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    repo.run_jj(&["describe", "a", "-m", "Add a, amended"]);
    submit(&repo, &forge, "b", &[]);
    let main = remote_head(repo.origin_path(), "main").unwrap();
    let ran = Command::new("git")
        .args(["update-ref", "refs/heads/b", &main])
        .current_dir(repo.origin_path())
        .status()
        .unwrap();
    assert!(ran.success());
    let message = err(undo(&repo, &forge, true));
    assert!(message.contains("'b' changed on origin"), "{message}");
    assert!(message.contains("so it changed nothing"), "{message}");
    assert_eq!(remote_head(repo.origin_path(), "b"), Some(main));
}

#[test]
fn a_base_someone_retargeted_needs_force() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    // Reorder: b's PR now targets main, so the second submit retargets it.
    forge.with_pr(2, |p| p.base = "main".into()).unwrap();
    submit(&repo, &forge, "b", &[]);
    assert_eq!(forge.pr(2).base, "a");
    forge.with_pr(2, |p| p.base = "dev".into()).unwrap();
    let message = err(undo(&repo, &forge, false));
    assert!(
        message.contains("#2's base was changed to 'dev' after jjpr set it"),
        "{message}"
    );
    assert!(message.contains("jjpr undo --force"), "{message}");
    let out = ok(undo(&repo, &forge, true));
    assert!(
        out.contains(
            "Warning: #2's base was changed to 'dev' after jjpr set it; restoring it anyway."
        ),
        "{out}"
    );
    assert_eq!(forge.pr(2).base, "main");
}

#[test]
fn review_requests_are_withdrawn_and_named() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &["alice".to_string()]);
    assert_eq!(forge.pr(1).reviewers, vec!["alice"]);
    let out = ok(undo(&repo, &forge, true));
    assert!(
        out.contains("Withdraw the review request to alice on #1"),
        "{out}"
    );
    assert!(
        out.contains("alice already had the review request on #1"),
        "{out}"
    );
    assert!(forge.pr(1).reviewers.is_empty());
}

#[test]
fn a_dry_run_changes_nothing() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let op = head_op(&repo);
    let out = ok(act(&repo, &forge, Direction::Undo, true, true));
    assert!(out.starts_with("Would undo `jjpr submit`"), "{out}");
    assert!(out.contains("Close #1, which the submit opened"), "{out}");
    assert_eq!(head_op(&repo), op);
    assert!(forge.pr(1).open && !forge.comments_on(1).is_empty());
    assert_eq!(entries(&repo)[0].state, jjpr::undo::journal::State::Done);
}

#[test]
fn a_merge_cannot_be_undone_and_ends_the_history_before_it() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    {
        let rec = recorder(&repo, "merge");
        let _jj = RecordingJj::new(repo.runner(), rec.clone());
        let recording = RecordingForge::new(Box::new(forge.clone()), rec);
        recording
            .merge_pr("o", "r", 1, MergeMethod::Squash)
            .unwrap();
    }
    assert_eq!(entries(&repo).len(), 1, "the submit before it is pruned");
    let message = err(undo(&repo, &forge, true));
    assert!(
        message.contains("it merged #1, and a merge can't be undone"),
        "{message}"
    );
}

#[test]
fn an_operation_jj_no_longer_has_is_refused_and_pruned() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    // As if `jj op abandon` or garbage collection had dropped it.
    let journal = Journal::for_repo(repo.path()).unwrap();
    let mut entry = entries(&repo).remove(0);
    entry.start_op = "0123456789abcdef".repeat(8);
    journal.save(&entry).unwrap();
    let message = err(undo(&repo, &forge, false));
    assert!(message.contains("its operation 0123456789ab."), "{message}");
    assert!(entries(&repo).is_empty());
}

#[test]
fn nothing_recorded_is_nothing_to_undo() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    assert_eq!(ok(undo(&repo, &forge, false)).trim(), "Nothing to undo.");
    assert_eq!(ok(redo(&repo, &forge)).trim(), "Nothing to redo.");
}

#[test]
fn a_running_watch_blocks_undo() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let path = repo.path().to_path_buf();
    let journal = Journal::for_repo(&path).unwrap();
    let jj_repo = JjRepo::new(path);
    let forge_for =
        |_: &jjpr::undo::journal::Entry| -> Result<Box<dyn Forge>> { Ok(Box::new(forge.clone())) };
    let cx = Context {
        journal: &journal,
        repo: &jj_repo,
        forge_for: &forge_for,
        watch_running: true,
        now: 0,
    };
    let opts = Options {
        direction: Direction::Undo,
        force: false,
        dry_run: false,
    };
    let message = format!(
        "{:#}",
        jjpr::undo::run(&cx, opts, &mut Vec::new()).unwrap_err()
    );
    assert!(
        message.contains("while `jjpr watch` is running"),
        "{message}"
    );
}

#[test]
fn a_command_that_changed_nothing_is_not_recorded() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    submit(&repo, &forge, "b", &[]);
    assert_eq!(
        entries(&repo).len(),
        1,
        "the second submit had nothing to do"
    );
}

/// The recording wrapper passes every `Jj` call through.
#[test]
fn the_recording_jj_reads_like_the_runner() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let runner = repo.runner();
    let rec = recorder(&repo, "submit");
    let jj = RecordingJj::new(repo.runner(), rec);
    assert_eq!(
        jj.get_my_bookmarks().unwrap(),
        runner.get_my_bookmarks().unwrap()
    );
    assert_eq!(jj.get_default_branch().unwrap(), "main");
    assert_eq!(
        jj.current_operation_id().unwrap(),
        runner.current_operation_id().unwrap()
    );
}

fn jjpr(repo: &common::JjTestRepo, args: &[&str]) -> std::process::Output {
    assert_cmd::cargo::cargo_bin_cmd!("jjpr")
        .args(args)
        .current_dir(repo.path())
        .output()
        .expect("test fixture")
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_cli_says_when_there_is_nothing_to_undo_or_redo() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let out = jjpr(&repo, &["undo"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), "Nothing to undo.\n");
    assert_eq!(stdout(&jjpr(&repo, &["redo"])), "Nothing to redo.\n");
    assert_eq!(
        stdout(&jjpr(&repo, &["undo", "--list"])),
        "No jjpr commands are recorded in this repo.\n"
    );
}

#[test]
fn the_cli_lists_recorded_commands_newest_first() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "a", &[]);
    submit(&repo, &forge, "b", &[]);
    ok(undo(&repo, &forge, true));
    let list = stdout(&jjpr(&repo, &["undo", "--list"]));
    let lines: Vec<&str> = list.lines().collect();
    assert_eq!(
        lines[0],
        "jjpr commands recorded in this repo, newest first:"
    );
    assert!(
        lines[1].contains("submit") && lines[1].contains("#1 #2"),
        "{list}"
    );
    assert!(lines[1].ends_with("undone"), "{list}");
    assert!(
        lines[2].contains("submit") && lines[2].contains("#1"),
        "{list}"
    );
    assert!(!lines[2].ends_with("undone"), "{list}");
}

#[test]
fn the_cli_refuses_while_watch_runs() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        repo.path().join(".jj/jjpr-watch.json"),
        format!(r#"{{"pid":1,"started_at":{now},"last_seen":{now}}}"#),
    )
    .unwrap();
    let out = jjpr(&repo, &["undo"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("while `jjpr watch` is running"), "{err}");
}

/// An amended resubmit whose push closed the PR (the forge does when nothing
/// is left to merge), recorded as if on `forge`.
fn closed_by_push(repo: &common::JjTestRepo, forge: &ModelForge, kind: ForgeKind) {
    submit(repo, forge, "a", &[]);
    repo.run_jj(&["describe", "a", "-m", "Add a, amended"]);
    forge.model.lock().expect("model lock").close_on_read = Some(1);
    submit(repo, forge, "a", &[]);
    assert!(!forge.pr(1).open, "the push closed it");
    let journal = Journal::for_repo(repo.path()).expect("test fixture");
    let mut last = entries(repo).pop().expect("test fixture");
    last.forge = kind;
    journal.save(&last).expect("test fixture");
}

/// GitHub will not reopen a PR whose branch moved while it was closed, which
/// undoing the push would need: undo refuses whole and changes nothing.
#[test]
fn a_pr_github_will_not_reopen_refuses_the_whole_undo() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    closed_by_push(&repo, &forge, ForgeKind::GitHub);
    let head = remote_head(repo.origin_path(), "a");
    for force in [false, true] {
        let message = err(undo(&repo, &forge, force));
        assert!(
            message.contains("the push closed #1, and GitHub won't reopen"),
            "{message}"
        );
        assert!(
            message.contains("Use jj to get the stack into the state you want"),
            "{message}"
        );
    }
    assert_eq!(remote_head(repo.origin_path(), "a"), head, "nothing pushed");
}

/// Elsewhere undo reopens a PR its push closed.
#[test]
fn a_pr_the_push_closed_is_reopened() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    closed_by_push(&repo, &forge, ForgeKind::GitLab);
    let out = ok(undo(&repo, &forge, false));
    assert!(out.contains("Reopen !1, which the push closed"), "{out}");
    assert!(forge.pr(1).open);
}

/// A reviewer closed the PR after the push: undo leaves it closed.
#[test]
fn a_pr_someone_closed_stays_closed() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "a", &[]);
    repo.run_jj(&["describe", "a", "-m", "Add a, amended"]);
    submit(&repo, &forge, "a", &[]);
    forge.with_pr(1, |p| p.open = false).unwrap();
    let out = ok(undo(&repo, &forge, false));
    assert!(!out.contains("Reopen"), "{out}");
    assert!(!forge.pr(1).open);
}

/// Comments posted again by one undo keep their new ids for the next, both
/// ways: create in the first submit, rewrite in the second, undo both, redo
/// both, and the comment is as the second submit left it, once.
#[test]
fn comments_survive_two_undos_and_two_redos() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    // A third bookmark, committed up front: submitting 'b' and then 'c' grows
    // the stack, so the second submit rewrites #1's stack comment, with
    // nothing changed in between.
    repo.commit_and_bookmark("c.rs", "// c\n", "Add c", "c");
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let first = forge.comments_on(1);
    submit(&repo, &forge, "c", &[]);
    let second = forge.comments_on(1);
    assert_ne!(first, second);
    assert_eq!(entries(&repo).len(), 2);
    ok(undo(&repo, &forge, true));
    assert_eq!(forge.comments_on(1), first);
    ok(undo(&repo, &forge, true));
    assert!(forge.comments_on(1).is_empty());
    ok(redo(&repo, &forge));
    assert_eq!(forge.comments_on(1), first);
    ok(redo(&repo, &forge));
    assert_eq!(forge.comments_on(1), second);
    ok(undo(&repo, &forge, true));
    assert_eq!(forge.comments_on(1), first, "and back again");
}

/// A process that dies mid-command (Ctrl-C) leaves its entry running; undo
/// finishes the record from what was saved and goes ahead.
#[test]
fn an_entry_whose_process_died_is_still_undone() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let journal = Journal::for_repo(repo.path()).unwrap();
    let mut entry = entries(&repo).remove(0);
    journal.remove(&entry.id).unwrap();
    entry.id = jjpr::undo::journal::new_id(1, 99_999_999);
    entry.state = jjpr::undo::journal::State::Running;
    entry.end_view = None;
    journal.save(&entry).unwrap();
    let out = ok(undo(&repo, &forge, true));
    assert!(out.contains("stopped before it finished"), "{out}");
    assert!(!forge.pr(1).open);
}

/// A dry run changes no journal file, even when it finds the entry's
/// operation gone.
#[test]
fn a_dry_run_prunes_nothing() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let journal = Journal::for_repo(repo.path()).unwrap();
    let mut entry = entries(&repo).remove(0);
    entry.start_op = "0123456789abcdef".repeat(8);
    journal.save(&entry).unwrap();
    err(act(&repo, &forge, Direction::Undo, false, true));
    assert_eq!(entries(&repo).len(), 1);
}

/// An entry in which jjpr wrote a comment and then removed it, built by hand
/// on the repo as it is now.
fn comment_entry(repo: &common::JjTestRepo, actions: Vec<Action>) {
    use jjpr::undo::UndoRepo;
    let jj = JjRepo::new(repo.path().to_path_buf());
    let op = jj.current_op().expect("test fixture");
    let entry = jjpr::undo::journal::Entry {
        schema: jjpr::undo::journal::SCHEMA,
        id: jjpr::undo::journal::new_id(1, std::process::id()),
        command: "submit".into(),
        started_at: 0,
        remote: "origin".into(),
        forge: ForgeKind::GitHub,
        owner: "o".into(),
        repo: "r".into(),
        start_op: op.clone(),
        end_op: Some(op.clone()),
        end_view: Some(jj.view_fingerprint_at(&op).expect("test fixture")),
        absorbed: vec![],
        state: jjpr::undo::journal::State::Done,
        local_undone: false,
        undone_view: None,
        last_op: None,
        closed_by_push: vec![],
        missed: vec![],
        actions: actions
            .into_iter()
            .map(|action| jjpr::undo::journal::Record {
                action,
                confirmed: true,
                undone: false,
            })
            .collect(),
    };
    Journal::for_repo(repo.path())
        .expect("test fixture")
        .save(&entry)
        .expect("test fixture");
}

/// Created and deleted in one command: undo posts the deleted one back, then
/// deletes it as the created one, leaving nothing, and redo does it again.
#[test]
fn a_comment_created_and_deleted_in_one_command_leaves_nothing() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    forge
        .create_pr("o", "r", "t", "b", "a", "main", false)
        .unwrap();
    comment_entry(
        &repo,
        vec![
            Action::CommentCreate {
                pr: 1,
                id: 9,
                body: "nav".into(),
            },
            Action::CommentDelete {
                pr: 1,
                id: 9,
                body: "nav".into(),
            },
        ],
    );
    let out = ok(undo(&repo, &forge, false));
    assert!(out.contains("Put back the stack comment on #1"), "{out}");
    assert!(out.contains("Delete the stack comment on #1"), "{out}");
    assert!(forge.comments_on(1).is_empty(), "{out}");
    ok(redo(&repo, &forge));
    assert!(forge.comments_on(1).is_empty());
}

/// Edited and then deleted in one command: undo posts it back and edits it
/// back, without calling it gone.
#[test]
fn a_comment_edited_and_deleted_in_one_command_comes_back_as_it_was() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    forge
        .create_pr("o", "r", "t", "b", "a", "main", false)
        .unwrap();
    comment_entry(
        &repo,
        vec![
            Action::CommentUpdate {
                pr: 1,
                id: 9,
                before: "old".into(),
                after: "mid".into(),
            },
            Action::CommentDelete {
                pr: 1,
                id: 9,
                body: "mid".into(),
            },
        ],
    );
    ok(undo(&repo, &forge, false));
    assert_eq!(forge.comments_on(1), vec!["old"]);
    ok(redo(&repo, &forge));
    assert!(forge.comments_on(1).is_empty());
}

/// Another jjpr command recording right now (its entry is running, its
/// process alive) blocks undo.
#[test]
fn a_command_recording_now_blocks_undo() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    let journal = Journal::for_repo(repo.path()).unwrap();
    let mut running = entries(&repo).remove(0);
    running.id = jjpr::undo::journal::new_id(u128::MAX >> 8, 1);
    running.state = jjpr::undo::journal::State::Running;
    journal.save(&running).unwrap();
    let message = err(undo(&repo, &forge, false));
    assert!(
        message.contains("another jjpr command is changing this repo (pid 1)"),
        "{message}"
    );
}

/// A watch running in another workspace of the repo leaves a marker in the
/// shared store, and undo in this one sees it.
#[test]
fn a_watch_in_another_workspace_blocks_undo() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let store = jjpr::undo::journal::repo_store_dir(repo.path()).unwrap();
    std::fs::create_dir_all(store.join("jjpr")).unwrap();
    std::fs::write(store.join("jjpr/watch-1"), "1").unwrap();
    assert!(jjpr::heartbeat::watch_running(repo.path()));
    std::fs::remove_file(store.join("jjpr/watch-1")).unwrap();
    std::fs::write(store.join("jjpr/watch-99999999"), "").unwrap();
    assert!(
        !jjpr::heartbeat::watch_running(repo.path()),
        "a dead watcher's marker"
    );
    assert!(
        !store.join("jjpr/watch-99999999").exists(),
        "and it is cleared, so a reused pid cannot revive it"
    );
}

/// A close that went through but answered with an error is not put back;
/// the check afterwards names it.
#[test]
fn a_write_that_landed_despite_its_error_is_named_afterwards() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    forge.model.lock().unwrap().close_lands_then_fails = true;
    let message = err(undo(&repo, &forge, true));
    assert!(message.contains("HTTP 504"), "{message}");
    assert!(
        message.contains("jjpr put back what it had changed, but these are not as they were:"),
        "{message}"
    );
    assert!(message.contains("#1 is closed (it was open)"), "{message}");
    assert!(
        message.contains("`jjpr undo --dry-run` shows where that leaves it."),
        "{message}"
    );
}

/// When the repo alone shows undo cannot go ahead, a forge that cannot be
/// reached does not hide that.
#[test]
fn a_local_blocker_is_reported_even_when_the_forge_is_unreachable() {
    if !common::jj_available() {
        return;
    }
    let repo = two_bookmarks();
    let forge = ModelForge::new(repo.origin_path());
    submit(&repo, &forge, "b", &[]);
    repo.run_jj(&["describe", "b", "-m", "Add b, later"]);
    let path = repo.path().to_path_buf();
    let journal = Journal::for_repo(&path).unwrap();
    let jj_repo = JjRepo::new(path);
    let forge_for = |_: &jjpr::undo::journal::Entry| -> Result<Box<dyn Forge>> {
        anyhow::bail!("could not resolve host")
    };
    let cx = Context {
        journal: &journal,
        repo: &jj_repo,
        forge_for: &forge_for,
        watch_running: false,
        now: 0,
    };
    let result = jjpr::undo::run(
        &cx,
        Options::new(Direction::Undo, true, false),
        &mut Vec::new(),
    );
    let message = format!("{:#}", result.unwrap_err());
    assert!(
        message.contains("the repo changed since, and that work would be lost"),
        "{message}"
    );
    assert!(
        message.contains("(jjpr could not check the forge as well: could not resolve host)"),
        "{message}"
    );
}
