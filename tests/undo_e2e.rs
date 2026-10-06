//! `jjpr undo` and `jjpr redo` against each real forge, through the binary.
//! Gated by `JJPR_E2E`, like `forge_e2e.rs`. Run serially:
//!
//!   JJPR_E2E=1 cargo test --test undo_e2e -- --test-threads=1 --nocapture

mod forge_e2e_harness;

use std::time::Duration;

use forge_e2e_harness::{ForgeE2eContext, OWNER, REPO, configured_drivers};

fn drivers() -> Vec<Box<dyn forge_e2e_harness::ForgeTestDriver>> {
    if !forge_e2e_harness::tool_available("jj") {
        return Vec::new();
    }
    configured_drivers()
}

/// Run jjpr, require success, and return what it printed.
fn jjpr(ctx: &ForgeE2eContext, args: &[&str]) -> String {
    let out = ctx.run_jjpr(args);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{}: jjpr {args:?}: {stdout}{}",
        ctx.driver.name(),
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

fn find_pr(ctx: &ForgeE2eContext, bookmark: &str) -> u64 {
    let head = ctx.prefixed(bookmark);
    for _ in 0..8 {
        if let Some(n) = ctx.driver.find_request_by_head(&head) {
            return n;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    panic!("no PR found for head '{head}'");
}

fn local(ctx: &ForgeE2eContext, bookmark: &str) -> String {
    let name = ctx.prefixed(bookmark);
    ctx.run_jj(&["log", "--no-graph", "-r", &name, "-T", "commit_id"])
        .trim()
        .to_string()
}

/// The branch head on the forge, polled until it is `want` (pushes and
/// deletions take a moment to show), or the last value seen.
fn branch_until(ctx: &ForgeE2eContext, bookmark: &str, want: Option<&str>) -> Option<String> {
    let forge = ctx.driver.jjpr_forge();
    let name = ctx.prefixed(bookmark);
    let mut last = None;
    for _ in 0..10 {
        last = forge.get_branch_head(OWNER, REPO, &name).unwrap();
        if last.as_deref() == want {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    last
}

fn state_until(ctx: &ForgeE2eContext, number: u64, want: &str) -> String {
    let mut last = String::new();
    for _ in 0..10 {
        last = ctx.driver.request_state(number);
        if last == want {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    last
}

/// Submit, amend, submit again; undo puts the forge back to the first push and
/// keeps the amend locally, and redo pushes the amend again.
#[test]
fn a_resubmit_is_undone_and_redone_on_every_forge() {
    for driver in drivers() {
        let name = driver.name();
        eprintln!("=== undo a resubmit: {name} ===");
        let ctx = ForgeE2eContext::new(driver);
        let p = ctx.prefix.clone();
        ctx.commit_bookmark("ua", "ua.txt", &title(&p, "undo base"));
        ctx.commit_bookmark("ub", "ub.txt", &title(&p, "undo leaf"));
        jjpr(&ctx, &["submit", &ctx.prefixed("ub")]);
        let (first_a, first_b) = (local(&ctx, "ua"), local(&ctx, "ub"));
        let (pa, pb) = (find_pr(&ctx, "ua"), find_pr(&ctx, "ub"));

        ctx.run_jj(&[
            "describe",
            &ctx.prefixed("ua"),
            "-m",
            &format!("undo base, amended {p}"),
        ]);
        let (amended_a, amended_b) = (local(&ctx, "ua"), local(&ctx, "ub"));
        jjpr(&ctx, &["submit", &ctx.prefixed("ub")]);
        assert_eq!(
            branch_until(&ctx, "ua", Some(&amended_a)).as_deref(),
            Some(amended_a.as_str()),
            "{name}"
        );

        let out = jjpr(&ctx, &["undo"]);
        assert!(out.contains("Force-push"), "{name}: {out}");
        assert_eq!(
            branch_until(&ctx, "ua", Some(&first_a)).as_deref(),
            Some(first_a.as_str()),
            "{name}"
        );
        assert_eq!(
            branch_until(&ctx, "ub", Some(&first_b)).as_deref(),
            Some(first_b.as_str()),
            "{name}"
        );
        assert_eq!(
            local(&ctx, "ua"),
            amended_a,
            "{name}: the amend stays local"
        );
        assert_eq!(state_until(&ctx, pa, "open"), "open", "{name}");
        assert_eq!(state_until(&ctx, pb, "open"), "open", "{name}");

        jjpr(&ctx, &["redo"]);
        assert_eq!(
            branch_until(&ctx, "ua", Some(&amended_a)).as_deref(),
            Some(amended_a.as_str()),
            "{name}"
        );
        assert_eq!(
            branch_until(&ctx, "ub", Some(&amended_b)).as_deref(),
            Some(amended_b.as_str()),
            "{name}"
        );
        eprintln!("=== {name}: undo a resubmit OK ===");
    }
}

/// Undo of a first submit leaves the new PRs open, `--force` closes them and
/// deletes their branches, and redo pushes the branches and reopens the PRs.
#[test]
fn created_prs_close_only_with_force_and_reopen_on_redo_on_every_forge() {
    for driver in drivers() {
        let name = driver.name();
        eprintln!("=== undo created PRs: {name} ===");
        let ctx = ForgeE2eContext::new(driver);
        let p = ctx.prefix.clone();
        ctx.commit_bookmark("ca", "ca.txt", &title(&p, "created base"));
        ctx.commit_bookmark("cb", "cb.txt", &title(&p, "created leaf"));
        jjpr(&ctx, &["submit", &ctx.prefixed("cb")]);
        let (pa, pb) = (find_pr(&ctx, "ca"), find_pr(&ctx, "cb"));
        let head_a = local(&ctx, "ca");

        let out = jjpr(&ctx, &["undo"]);
        assert!(out.contains("jjpr undo --force"), "{name}: {out}");
        assert_eq!(state_until(&ctx, pa, "open"), "open", "{name}");
        assert!(branch_until(&ctx, "ca", Some(&head_a)).is_some(), "{name}");

        let out = jjpr(&ctx, &["undo", "--force"]);
        assert!(out.contains("Close"), "{name}: {out}");
        assert_eq!(state_until(&ctx, pa, "closed"), "closed", "{name}");
        assert_eq!(state_until(&ctx, pb, "closed"), "closed", "{name}");
        assert_eq!(branch_until(&ctx, "ca", None), None, "{name}");
        assert_eq!(branch_until(&ctx, "cb", None), None, "{name}");

        let out = ctx.run_jjpr(&["redo"]);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!("--- {name}: redo after the branches were deleted:\n{said}");
        assert!(out.status.success(), "{name}: {said}");
        assert_eq!(
            branch_until(&ctx, "ca", Some(&head_a)).as_deref(),
            Some(head_a.as_str()),
            "{name}"
        );
        assert_eq!(state_until(&ctx, pa, "open"), "open", "{name}");
        assert_eq!(state_until(&ctx, pb, "open"), "open", "{name}");
        eprintln!("=== {name}: undo created PRs OK ===");
    }
}

/// The restack after an out-of-band squash merge: a rebase, an abandon and a
/// push. One undo puts the stack and the forge back.
#[test]
fn a_restack_is_undone_on_every_forge() {
    for driver in drivers() {
        let name = driver.name();
        eprintln!("=== undo a restack: {name} ===");
        let ctx = ForgeE2eContext::new(driver);
        let p = ctx.prefix.clone();
        ctx.commit_bookmark("rbot", "rb1.txt", &title(&p, "restack base one"));
        ctx.commit_bookmark("rbot", "rb2.txt", &title(&p, "restack base two"));
        ctx.commit_bookmark("rtop", "rtop.txt", &title(&p, "restack leaf"));
        let top = ctx.prefixed("rtop");
        jjpr(&ctx, &["submit", &top]);
        let bottom_pr = find_pr(&ctx, "rbot");
        let before_restack = local(&ctx, "rtop");
        ctx.driver.admin_squash_deleting_branch(bottom_pr);
        ctx.run_jj(&["git", "fetch"]);

        let out = jjpr(&ctx, &["submit", &top]);
        assert!(
            out.contains(&format!("Rebasing '{top}' onto main")),
            "{name}: {out}"
        );
        let restacked = local(&ctx, "rtop");
        assert_ne!(restacked, before_restack, "{name}");

        let out = jjpr(&ctx, &["undo"]);
        assert!(out.contains("Restore the local repo"), "{name}: {out}");
        assert_eq!(
            local(&ctx, "rtop"),
            before_restack,
            "{name}: the stack is back"
        );
        assert_eq!(
            branch_until(&ctx, "rtop", Some(&before_restack)).as_deref(),
            Some(before_restack.as_str()),
            "{name}"
        );

        jjpr(&ctx, &["redo"]);
        assert_eq!(local(&ctx, "rtop"), restacked, "{name}");
        assert_eq!(
            branch_until(&ctx, "rtop", Some(&restacked)).as_deref(),
            Some(restacked.as_str()),
            "{name}"
        );
        eprintln!("=== {name}: undo a restack OK ===");
    }
}

/// Settles what the design left open, on each forge: whether jjpr's draft and
/// ready calls take effect, and when a closed PR can be reopened. Prints each
/// answer; asserts the ones undo relies on.
#[test]
fn draft_round_trip_and_reopen_rules_on_every_forge() {
    for driver in drivers() {
        let name = driver.name();
        let ctx = ForgeE2eContext::new(driver);
        let p = ctx.prefix.clone();
        let forge = ctx.driver.jjpr_forge();
        let draft_of = |pr: u64| {
            std::thread::sleep(Duration::from_secs(2));
            forge.get_pr(OWNER, REPO, pr).unwrap().0.draft
        };

        ctx.commit_bookmark("dq", "dq.txt", &title(&p, "draft probe"));
        jjpr(&ctx, &["submit", &ctx.prefixed("dq")]);
        let pr = find_pr(&ctx, "dq");
        forge.convert_to_draft(OWNER, REPO, pr).unwrap();
        let drafted = draft_of(pr);
        forge.mark_pr_ready(OWNER, REPO, pr).unwrap();
        let readied = !draft_of(pr);
        eprintln!("--- {name}: draft took: {drafted}; ready took: {readied}");

        // Closed with its branch untouched, then reopened.
        forge.close_pr(OWNER, REPO, pr).unwrap();
        state_until(&ctx, pr, "closed");
        let untouched = forge.reopen_pr(OWNER, REPO, pr);
        eprintln!("--- {name}: reopen, branch untouched: {untouched:?}");

        // Closed, branch deleted and pushed again at the same commit.
        forge.close_pr(OWNER, REPO, pr).unwrap();
        state_until(&ctx, pr, "closed");
        let dq = ctx.prefixed("dq");
        let commit = local(&ctx, "dq");
        ctx.run_jj(&["bookmark", "delete", &dq]);
        ctx.run_jj(&["git", "push", "--bookmark", &dq]);
        ctx.run_jj(&["bookmark", "set", &dq, "-r", &commit]);
        ctx.push("dq");
        let recreated = forge.reopen_pr(OWNER, REPO, pr);
        eprintln!("--- {name}: reopen, branch deleted and recreated: {recreated:?}");

        // Closed, branch force-pushed while closed.
        let _ = forge.close_pr(OWNER, REPO, pr);
        state_until(&ctx, pr, "closed");
        ctx.run_jj(&["describe", &dq, "-m", &format!("draft probe, amended {p}")]);
        ctx.run_jj(&["git", "push", "--bookmark", &dq]);
        let forced = forge.reopen_pr(OWNER, REPO, pr);
        eprintln!("--- {name}: reopen, branch force-pushed: {forced:?}");

        assert!(
            drafted && readied,
            "{name}: draft {drafted}, ready {readied}"
        );
        assert!(untouched.is_ok(), "{name}: {untouched:?}");
    }
}

/// A commit message, and so a PR title, unlike any other run's. Codeberg
/// refuses (429) a PR whose title is too like two others in the last hour,
/// and a run prefix alone does not make it different enough.
fn title(prefix: &str, what: &str) -> String {
    const WORDS: [&str; 16] = [
        "amber", "birch", "cobalt", "delta", "ember", "fjord", "garnet", "harbor", "indigo",
        "juniper", "kestrel", "lagoon", "marble", "nectar", "orchid", "pewter",
    ];
    let seed: usize = prefix.bytes().map(usize::from).sum::<usize>()
        + std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as usize)
            .unwrap_or(0);
    let word = |i: usize| WORDS[(seed / (i + 1) + i * 7) % WORDS.len()];
    format!("{} {} {what} {prefix}", word(0), word(1))
}
