//! Multi-forge e2e harness.
//!
//! A `ForgeTestDriver` abstracts the forge-specific setup/teardown/assert
//! operations an e2e test needs (open a PR/MR, read its head SHA/base/state,
//! admin-merge by method, configure a temporary dismiss-stale protection, clean
//! up). Feature tests drive jjpr itself for the behavior under test and use the
//! driver only for the forge scaffolding, so one test body runs on every forge.
//!
//! Isolation rules (see the `forge-e2e-testing` skill): everything a run creates
//! is namespaced with a unique prefix; cleanup only ever touches that prefix;
//! protection is scoped to prefixed throwaway branches and removed on teardown;
//! `main` is never persistently protected. All three forges share one repo
//! (`OWNER/REPO`), so breaking these corrupts other projects' fixtures.

// Harness scaffolding: some driver ops (e.g. Rebase merges, default_branch) are
// exercised by the feature tests, not yet by the smoke test.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

mod forgejo;
mod github;
mod gitlab;

/// Owner/repo of the shared sandbox, identical on every forge.
pub const OWNER: &str = "michaeldhopkins";
pub const REPO: &str = "forge-e2e-sandbox";

/// How the bottom PR of a stack is landed. Matters because a merge-commit keeps
/// the base commit in trunk (descendant needs no rebase) while a squash drops it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeMethod {
    MergeCommit,
    Squash,
    Rebase,
}

/// Forge-specific harness operations. One impl per forge.
pub trait ForgeTestDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// SSH clone URL for `jj git clone --colocate`.
    fn clone_url(&self) -> String;
    fn default_branch(&self) -> &'static str {
        "main"
    }
    /// Open a PR/MR for an already-pushed `head` branch targeting `base`.
    /// Returns its number.
    fn open_request(&self, head: &str, base: &str, title: &str) -> u64;
    fn find_request_by_head(&self, head: &str) -> Option<u64>;
    fn request_head_sha(&self, number: u64) -> String;
    fn request_base(&self, number: u64) -> String;
    /// `open` | `merged` | `closed`.
    fn request_state(&self, number: u64) -> String;
    /// A fresh boxed instance of this driver. Drivers are stateless, so this
    /// lets one forge back several independent test contexts (each its own
    /// clone + prefix) in a single test.
    fn boxed(&self) -> Box<dyn ForgeTestDriver>;
    /// Mark a request as draft/WIP so jjpr treats it as blocked and leaves it
    /// open. Used to keep the descendant open while jjpr merges and reconciles
    /// the PR below it. Each forge has its own mechanism (a real draft flag on
    /// GitHub, a `Draft:`/`WIP:` title prefix on GitLab/Forgejo) — all of which
    /// jjpr reads back as `draft`.
    fn make_draft(&self, number: u64);
    /// Land the request, bypassing required reviews. `method` selects the
    /// merge strategy where the forge supports it.
    fn admin_merge(&self, number: u64, method: MergeMethod);
    /// Turn on "dismiss stale approvals on push" for `branch` (a prefixed,
    /// throwaway branch — never `main`). Must be paired with `remove_protection`
    /// in teardown so no standing protection is left on the shared repo.
    fn set_dismiss_stale(&self, branch: &str);
    fn remove_protection(&self, branch: &str);
    /// Whether dismiss-stale can be toggled on the shared sandbox. GitLab's
    /// reset-on-push is a Premium feature and can't be set on a free project, so
    /// its detection e2e is skipped (the parse logic is unit-tested instead).
    fn dismiss_stale_toggle_supported(&self) -> bool {
        true
    }
    /// Whether a squash landing reliably rewrites history (a new commit that
    /// orphans the descendant's parent) on the shared sandbox. Always true on
    /// GitHub; on GitLab/Forgejo a single-commit squash can fast-forward the
    /// original commit unchanged (config-dependent), so the "rebase happens"
    /// control is skipped there — the merge-commit skip (the actual feature) is
    /// still exercised, and the rebase direction is covered by GitHub + units.
    fn squash_rewrites_history(&self) -> bool {
        true
    }
    /// jjpr's own forge client for this forge — so feature tests exercise the
    /// real production code (e.g. `base_dismisses_stale_approvals`), not the
    /// harness's setup calls.
    fn jjpr_forge(&self) -> Box<dyn jjpr::forge::Forge>;
    /// Close prefixed requests and delete prefixed refs and any protection this
    /// run created. Only ever touches `prefix` — never bulk operations.
    fn cleanup_prefix(&self, prefix: &str);
}

/// A cloned working copy with a unique run prefix, cleaned up on `Drop`.
pub struct ForgeE2eContext {
    pub driver: Box<dyn ForgeTestDriver>,
    pub prefix: String,
    pub repo_path: PathBuf,
    _parent: TempDir,
}

impl ForgeE2eContext {
    pub fn new(driver: Box<dyn ForgeTestDriver>) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        // A per-run sequence disambiguates contexts created in the same second by
        // the same process (a single test spins up several).
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_secs();
        let pid = std::process::id() as u64;
        // Forge letter + time + pid + seq keeps prefixes short and collision-free
        // across forges, parallel runs, and multiple contexts in one test. Keep
        // the full 20 bits of pid — a per-context seq disambiguates within a
        // process without spending pid entropy that guards across processes.
        let prefix = format!(
            "{}{:05x}{:05x}{:03x}",
            &driver.name()[..1],
            ts & 0xFFFFF,
            pid & 0xFFFFF,
            seq & 0xFFF,
        );

        let parent = TempDir::new().expect("temp dir");
        let repo_path = parent.path().join("repo");
        let dest = repo_path.to_str().expect("utf8 path");
        let url = driver.clone_url();
        let out = Command::new("jj")
            .args(["git", "clone", "--colocate", &url, dest])
            .output()
            .expect("jj git clone");
        assert!(out.status.success(), "clone {url} failed: {}", String::from_utf8_lossy(&out.stderr));

        Self { driver, prefix, repo_path, _parent: parent }
    }

    pub fn prefixed(&self, name: &str) -> String {
        format!("{}-{}", self.prefix, name)
    }

    pub fn run_jj(&self, args: &[&str]) -> String {
        let out = Command::new("jj")
            .args(args)
            .current_dir(&self.repo_path)
            .output()
            .expect("run jj");
        assert!(out.status.success(), "jj {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Create one commit adding `file` and set the prefixed `bookmark` on it.
    pub fn commit_bookmark(&self, bookmark: &str, file: &str, message: &str) {
        let f = self.prefixed(file);
        std::fs::write(self.repo_path.join(&f), format!("{f}\n")).expect("write file");
        self.run_jj(&["commit", "-m", message]);
        self.run_jj(&["bookmark", "set", &self.prefixed(bookmark), "-r", "@-"]);
    }

    /// Push a prefixed bookmark to the remote.
    pub fn push(&self, bookmark: &str) {
        self.run_jj(&["git", "push", "--bookmark", &self.prefixed(bookmark), "--allow-new"]);
    }

    /// Run the `jjpr` binary in this clone. Inherits the environment so the
    /// binary's token resolution falls back to the same `gh` / `glab` /
    /// `FORGEJO_TOKEN` the drivers already use, and detects the forge from the
    /// clone's remote. Returns the completed output for the caller to assert on.
    pub fn run_jjpr(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_jjpr"))
            .args(args)
            .current_dir(&self.repo_path)
            .output()
            .expect("run jjpr")
    }
}

impl Drop for ForgeE2eContext {
    fn drop(&mut self) {
        self.driver.cleanup_prefix(&self.prefix);
    }
}

/// Drivers for the forges that are both enabled (`JJPR_E2E`) and reachable
/// (CLI/token present). A forge without its tool/token is silently skipped so a
/// partial local setup still runs what it can. Empty when `JJPR_E2E` is unset.
pub fn configured_drivers() -> Vec<Box<dyn ForgeTestDriver>> {
    if std::env::var("JJPR_E2E").is_err() {
        eprintln!("Skipping forge e2e (set JJPR_E2E=1 to run)");
        return Vec::new();
    }
    let mut drivers: Vec<Box<dyn ForgeTestDriver>> = Vec::new();
    if github::available() {
        drivers.push(Box::new(github::GitHubDriver));
    } else {
        eprintln!("Skipping GitHub e2e (gh not available/authed)");
    }
    if gitlab::available() {
        drivers.push(Box::new(gitlab::GitLabDriver));
    } else {
        eprintln!("Skipping GitLab e2e (glab not available/authed)");
    }
    if forgejo::available() {
        drivers.push(Box::new(forgejo::ForgejoDriver));
    } else {
        eprintln!("Skipping Forgejo e2e (FORGEJO_TOKEN unset)");
    }
    drivers
}

/// Whether a command exists and `--version` succeeds.
pub(crate) fn tool_available(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok_and(|o| o.status.success())
}
