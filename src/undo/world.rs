//! An in-memory repo and forge for testing the executor: every effect undo
//! can have, and a switch that makes the n-th one fail. Test-only.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use anyhow::{Result, bail};

use crate::forge::Forge;
use crate::forge::types::{
    ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequest, PullRequestRef,
    ReviewSummary,
};

use super::plan::{Observed, SeenPr, Status};
use super::repo::{Operation, Targets, UndoRepo};

#[derive(Debug, Clone, Default)]
struct State {
    /// Each operation's local bookmarks; the last is the current one.
    ops: Vec<BTreeMap<String, String>>,
    forge: Observed,
    next_comment: u64,
    /// Effects (restores, pushes, forge writes) made so far.
    effects: usize,
}

/// Which effects fail: the one numbered `at`, or every one from `from` on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Failing {
    pub at: Option<usize>,
    pub from: Option<usize>,
}

pub struct World {
    state: Mutex<State>,
    pub failing: Mutex<Failing>,
}

impl World {
    /// A repo with `local` bookmarks at its one operation, and `forge`.
    pub fn new(local: BTreeMap<String, String>, forge: Observed) -> Self {
        Self {
            state: Mutex::new(State {
                ops: vec![local],
                forge,
                next_comment: 1000,
                effects: 0,
            }),
            failing: Mutex::new(Failing::default()),
        }
    }

    pub fn fail(&self, failing: Failing) {
        *self.failing.lock().unwrap() = failing;
    }

    /// What the forge holds now.
    pub fn forge(&self) -> Observed {
        self.state.lock().unwrap().forge.clone()
    }

    pub fn local(&self) -> BTreeMap<String, String> {
        self.state
            .lock()
            .unwrap()
            .ops
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// Count an effect, failing it when asked to.
    fn effect(&self) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        let n = s.effects;
        s.effects += 1;
        let f = *self.failing.lock().unwrap();
        if f.at == Some(n) || f.from.is_some_and(|from| n >= from) {
            bail!("HTTP 502: effect {n} failed");
        }
        Ok(())
    }

    fn write<T>(&self, f: impl FnOnce(&mut Observed) -> Result<T>) -> Result<T> {
        self.effect()?;
        f(&mut self.state.lock().unwrap().forge)
    }

    fn pr_write(&self, n: u64, change: impl FnOnce(&mut SeenPr)) -> Result<()> {
        self.write(|f| {
            change(pr_mut(f, n)?);
            Ok(())
        })
    }

    fn new_op(&self, change: impl FnOnce(&mut BTreeMap<String, String>)) {
        let mut s = self.state.lock().unwrap();
        let mut next = s.ops.last().cloned().unwrap_or_default();
        change(&mut next);
        s.ops.push(next);
    }
}

fn pr_mut(forge: &mut Observed, number: u64) -> Result<&mut SeenPr> {
    match forge.prs.get_mut(&number) {
        Some(pr) => Ok(pr),
        None => bail!("HTTP 404: no PR {number}"),
    }
}

impl UndoRepo for World {
    fn files_changed_since(&self, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn working_copies(&self, _: Option<&str>) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
    fn own_working_copies(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn current_op(&self) -> Result<String> {
        Ok((self.state.lock().unwrap().ops.len() - 1).to_string())
    }
    fn snapshot(&self) -> Result<()> {
        Ok(())
    }
    fn view_fingerprint(&self) -> Result<String> {
        Ok(format!("{:?}", self.local()))
    }
    fn view_fingerprint_at(&self, op: &str) -> Result<String> {
        let s = self.state.lock().unwrap();
        Ok(format!("{:?}", s.ops[op.parse::<usize>()?]))
    }
    fn ops_since(&self, _: &str, _: usize) -> Result<Option<Vec<Operation>>> {
        Ok(None)
    }
    fn op_exists(&self, op: &str) -> Result<bool> {
        let s = self.state.lock().unwrap();
        Ok(op.parse::<usize>().is_ok_and(|i| i < s.ops.len()))
    }
    fn restore_repo_only(&self, op: &str) -> Result<()> {
        self.effect()?;
        let at = self.state.lock().unwrap().ops[op.parse::<usize>()?].clone();
        self.new_op(|local| *local = at);
        Ok(())
    }
    fn targets(&self, bookmark: &str, _: &str) -> Result<Targets> {
        let s = self.state.lock().unwrap();
        Ok(Targets {
            local: s.ops.last().and_then(|l| l.get(bookmark).cloned()),
            remote: s.forge.branches.get(bookmark).cloned().flatten(),
        })
    }
    fn set_bookmark(&self, bookmark: &str, commit: &str) -> Result<()> {
        self.new_op(|local| {
            local.insert(bookmark.to_string(), commit.to_string());
        });
        Ok(())
    }
    fn delete_bookmark(&self, bookmark: &str) -> Result<()> {
        self.new_op(|local| {
            local.remove(bookmark);
        });
        Ok(())
    }
    fn push_bookmark(&self, bookmark: &str, _: &str) -> Result<()> {
        self.effect()?;
        let to = self.local().get(bookmark).cloned();
        self.state
            .lock()
            .unwrap()
            .forge
            .branches
            .insert(bookmark.to_string(), to);
        Ok(())
    }
}

fn to_pr(number: u64, p: &SeenPr) -> PullRequest {
    let side = |r: &str| PullRequestRef {
        ref_name: r.to_string(),
        label: String::new(),
        sha: String::new(),
    };
    PullRequest {
        number,
        html_url: String::new(),
        title: String::new(),
        body: Some(p.body.clone()),
        base: side(&p.base),
        head: side(""),
        draft: p.draft,
        node_id: String::new(),
        merged_at: None,
        requested_reviewers: p.reviewers.clone(),
        author: "me".to_string(),
        stack: None,
    }
}

impl Forge for World {
    fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
        Ok(Vec::new())
    }
    fn create_pr(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: bool,
    ) -> Result<PullRequest> {
        bail!("not used by undo")
    }
    fn update_pr_base(&self, _: &str, _: &str, n: u64, base: &str) -> Result<()> {
        self.pr_write(n, |p| p.base = base.to_string())
    }
    fn request_reviewers(&self, _: &str, _: &str, n: u64, who: &[String]) -> Result<()> {
        self.pr_write(n, |p| p.reviewers.extend(who.iter().cloned()))
    }
    fn list_comments(&self, _: &str, _: &str, n: u64) -> Result<Vec<IssueComment>> {
        let s = self.state.lock().unwrap();
        Ok(s.forge
            .comments
            .get(&n)
            .map(|c| {
                c.iter()
                    .map(|(id, body)| IssueComment {
                        id: *id,
                        body: Some(body.clone()),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
    fn create_comment(&self, _: &str, _: &str, n: u64, body: &str) -> Result<IssueComment> {
        self.effect()?;
        let mut s = self.state.lock().unwrap();
        let id = s.next_comment;
        s.next_comment += 1;
        s.forge
            .comments
            .entry(n)
            .or_default()
            .insert(id, body.to_string());
        Ok(IssueComment {
            id,
            body: Some(body.to_string()),
        })
    }
    fn update_comment(&self, _: &str, _: &str, id: u64, body: &str) -> Result<()> {
        self.write(|f| {
            for comments in f.comments.values_mut() {
                if let Some(c) = comments.get_mut(&id) {
                    *c = body.to_string();
                    return Ok(());
                }
            }
            bail!("HTTP 404: no comment {id}")
        })
    }
    fn delete_comment(&self, _: &str, _: &str, id: u64) -> Result<()> {
        self.write(|f| {
            for comments in f.comments.values_mut() {
                if comments.remove(&id).is_some() {
                    return Ok(());
                }
            }
            bail!("HTTP 404: no comment {id}")
        })
    }
    fn update_pr_body(&self, _: &str, _: &str, n: u64, body: &str) -> Result<()> {
        self.pr_write(n, |p| p.body = body.to_string())
    }
    fn mark_pr_ready(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.pr_write(n, |p| p.draft = false)
    }
    fn get_authenticated_user(&self) -> Result<String> {
        Ok("me".into())
    }
    fn find_merged_pr(&self, _: &str, _: &str, _: &str) -> Result<Option<PullRequest>> {
        Ok(None)
    }
    fn merge_pr(&self, _: &str, _: &str, _: u64, _: MergeMethod) -> Result<()> {
        bail!("not used by undo")
    }
    fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> {
        Ok(ChecksStatus::None)
    }
    fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> {
        Ok(ReviewSummary {
            approved_count: 0,
            changes_requested: false,
        })
    }
    fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> {
        bail!("not used by undo")
    }
    fn get_pr_state(&self, _: &str, _: &str, n: u64) -> Result<PrState> {
        Ok(self.get_pr("", "", n)?.1)
    }
    fn get_pr(&self, _: &str, _: &str, n: u64) -> Result<(PullRequest, PrState)> {
        let s = self.state.lock().unwrap();
        let Some(p) = s.forge.prs.get(&n) else {
            bail!("HTTP 404: no PR {n}");
        };
        let state = PrState {
            merged: p.status == Status::Merged,
            state: if p.status == Status::Open {
                "open"
            } else {
                "closed"
            }
            .to_string(),
        };
        Ok((to_pr(n, p), state))
    }
    fn close_pr(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.pr_write(n, |p| p.status = Status::Closed)
    }
    fn reopen_pr(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.pr_write(n, |p| p.status = Status::Open)
    }
    fn convert_to_draft(&self, _: &str, _: &str, n: u64) -> Result<()> {
        self.pr_write(n, |p| p.draft = true)
    }
    fn remove_reviewers(&self, _: &str, _: &str, n: u64, who: &[String]) -> Result<()> {
        self.write(|f| {
            pr_mut(f, n)?
                .reviewers
                .retain(|r| !who.iter().any(|w| w.eq_ignore_ascii_case(r)));
            Ok(())
        })
    }
    fn get_branch_head(&self, _: &str, _: &str, branch: &str) -> Result<Option<String>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .forge
            .branches
            .get(branch)
            .cloned()
            .flatten())
    }
}

/// `HashMap` keys in a fixed order, for building worlds from generated data.
pub fn sorted<K: Ord + Clone, V>(map: &HashMap<K, V>) -> Vec<K> {
    let mut keys: Vec<K> = map.keys().cloned().collect();
    keys.sort();
    keys
}
