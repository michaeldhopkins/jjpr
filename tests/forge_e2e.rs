//! Multi-forge e2e harness smoke test.
//!
//! Proves each configured forge driver can do the full cycle a feature test
//! relies on: clone, push a prefixed branch, open a PR/MR, read its
//! state/base/head-sha, round-trip a scoped dismiss-stale protection,
//! admin-merge it, and clean up. Gated by `JJPR_E2E`; each forge is skipped if
//! its CLI/token is absent. Run serially (real-forge state races in parallel):
//!
//!   JJPR_E2E=1 cargo test --test forge_e2e -- --test-threads=1 --nocapture

mod forge_e2e_harness;

use forge_e2e_harness::{
    configured_drivers, ForgeE2eContext, ForgeTestDriver, MergeMethod, OWNER, REPO,
};

/// Poll jjpr's detection until it reports `want` (forge config propagates
/// asynchronously), returning the last value seen.
fn detect(forge: &dyn jjpr::forge::Forge, branch: &str, want: Option<bool>) -> Option<bool> {
    let mut got = None;
    for _ in 0..6 {
        got = forge.base_dismisses_stale_approvals(OWNER, REPO, branch).ok().flatten();
        if got == want {
            return got;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    got
}

#[test]
fn forge_harness_smoke_all_forges() {
    let drivers = configured_drivers();
    if drivers.is_empty() {
        return; // JJPR_E2E unset, or no forge configured
    }
    if !forge_e2e_harness::tool_available("jj") {
        eprintln!("Skipping: jj not available");
        return;
    }

    for driver in drivers {
        let name = driver.name();
        eprintln!("=== forge harness smoke: {name} ===");

        let ctx = ForgeE2eContext::new(driver);
        ctx.commit_bookmark("smoke", "smoke.txt", "e2e harness smoke");
        ctx.push("smoke");
        let head = ctx.prefixed("smoke");

        let num = ctx.driver.open_request(&head, "main", "e2e harness smoke");
        assert_eq!(ctx.driver.request_state(num), "open", "{name}: PR should be open");
        assert_eq!(ctx.driver.request_base(num), "main", "{name}: base should be main");
        assert!(!ctx.driver.request_head_sha(num).is_empty(), "{name}: head sha present");
        assert_eq!(ctx.driver.find_request_by_head(&head), Some(num), "{name}: find by head");

        // Scoped dismiss-stale protection round-trip (never touches main).
        ctx.driver.set_dismiss_stale(&head);
        ctx.driver.remove_protection(&head);

        // Land it, then confirm it reads as merged.
        ctx.driver.admin_merge(num, MergeMethod::MergeCommit);
        assert_eq!(ctx.driver.request_state(num), "merged", "{name}: should be merged");

        eprintln!("=== {name}: OK ===");
        // ForgeE2eContext::drop cleans up everything under this run's prefix.
    }
}

/// Feature 2 e2e: jjpr's `base_dismisses_stale_approvals` reads real per-forge
/// protection. Configure dismiss-stale on a prefixed branch, assert detection
/// sees it, remove it, assert detection sees it gone. No approval / second
/// account needed — this reads config only.
#[test]
fn feature2_dismiss_stale_detection_all_forges() {
    let drivers = configured_drivers();
    if drivers.is_empty() {
        return;
    }
    if !forge_e2e_harness::tool_available("jj") {
        return;
    }

    for driver in drivers {
        let name = driver.name();
        // Captured before the driver moves into the context.
        let toggleable = driver.dismiss_stale_toggle_supported();
        eprintln!("=== feature2 detection: {name} ===");

        let ctx = ForgeE2eContext::new(driver);
        // A real prefixed branch to protect (GitLab's setting is project-level,
        // so the branch is just a label there).
        ctx.commit_bookmark("dsbase", "ds.txt", "e2e dismiss-stale base");
        ctx.push("dsbase");
        let branch = ctx.prefixed("dsbase");
        let forge = ctx.driver.jjpr_forge();

        // Every forge: the real detection READ works against the live API and
        // reports "off" for an unprotected branch.
        assert_eq!(detect(&*forge, &branch, Some(false)), Some(false), "{name}: baseline off");

        if !toggleable {
            // Paywalled precondition (e.g. GitLab reset-on-push is Premium):
            // the "on" state can't be created on this account. jjpr still
            // handles it — the on-parse is unit-tested — and the read path above
            // is exercised e2e here. See the forge-e2e-testing skill.
            eprintln!("=== {name}: toggle unavailable (Premium); on-state unit-tested, read path e2e-verified ===");
            continue;
        }

        ctx.driver.set_dismiss_stale(&branch);
        assert_eq!(detect(&*forge, &branch, Some(true)), Some(true), "{name}: detect ON");
        ctx.driver.remove_protection(&branch);
        assert_eq!(detect(&*forge, &branch, Some(false)), Some(false), "{name}: detect OFF");

        eprintln!("=== {name}: detection round-trip OK ===");
    }
}

/// Poll until a PR appears for `bookmark`'s prefixed head (forge indexing after
/// `jjpr submit` is asynchronous).
fn find_pr(ctx: &ForgeE2eContext, bookmark: &str) -> u64 {
    let head = ctx.prefixed(bookmark);
    for _ in 0..8 {
        if let Some(n) = ctx.driver.find_request_by_head(&head) {
            return n;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    panic!("no PR found for head '{head}'");
}

/// The unchanged case is stable immediately; the changed case needs the
/// force-push to propagate, so poll until it differs. Returns the last SHA seen
/// so a failed expectation still reports a concrete value.
fn poll_head_sha(ctx: &ForgeE2eContext, number: u64, before: &str, expect_changed: bool) -> String {
    let mut last = ctx.driver.request_head_sha(number);
    if !expect_changed {
        return last;
    }
    for _ in 0..8 {
        if last != before {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        last = ctx.driver.request_head_sha(number);
    }
    last
}

/// Feature 1 e2e: when jjpr merges the bottom PR of a two-stack, its post-merge
/// reconcile force-pushes the descendant only when it genuinely must.
///
/// - **Merge-commit** landing leaves the bottom's commit in trunk, so the
///   descendant already sits on trunk: jjpr skips the rebase and force-push, and
///   the descendant's remote head SHA is **unchanged** (approvals would survive
///   under dismiss-stale).
/// - **Squash** landing orphans the descendant's parent, so the rebase and
///   force-push are unavoidable: the head SHA **changes** (the control case).
///
/// Either way jjpr retargets the descendant's base to trunk. To let jjpr merge
/// the bottom itself (the real Feature 1 code path) while leaving the descendant
/// open for inspection, the descendant is marked draft after submit — so jjpr
/// merges the bottom, reconciles the still-draft top, then stops at it.
#[test]
fn feature1_skip_rebase_preserves_descendant_sha_all_forges() {
    let drivers = configured_drivers();
    if drivers.is_empty() {
        return;
    }
    if !forge_e2e_harness::tool_available("jj") {
        return;
    }

    for driver in drivers {
        let name = driver.name();
        let squash_rewrites = driver.squash_rewrites_history();
        eprintln!("=== feature1 skip-rebase: {name} ===");
        // The feature: a merge-commit landing keeps the descendant's SHA.
        feature1_scenario(driver.boxed(), MergeMethod::MergeCommit, false, name);
        // Control: a squash landing that rewrites history forces the rebase.
        if squash_rewrites {
            feature1_scenario(driver.boxed(), MergeMethod::Squash, true, name);
        } else {
            eprintln!(
                "=== {name}: squash control skipped (single-commit squash fast-forwards; \
                 rebase direction covered by GitHub e2e + unit tests) ==="
            );
        }
        eprintln!("=== {name}: feature1 OK ===");
    }
}

fn feature1_scenario(
    driver: Box<dyn ForgeTestDriver>,
    method: MergeMethod,
    expect_sha_changed: bool,
    name: &str,
) {
    let method_flag = match method {
        MergeMethod::MergeCommit => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    };
    let ctx = ForgeE2eContext::new(driver);
    // A two-high stack rooted on trunk: bottom ← top. Titles are unique per run
    // and distinct from each other so Codeberg's "similarly named issues"
    // anti-spam throttle doesn't fire on the shared sandbox.
    ctx.commit_bookmark("f1bot", "f1bot.txt", &format!("f1 base {}", ctx.prefix));
    ctx.commit_bookmark("f1top", "f1top.txt", &format!("f1 leaf {}", ctx.prefix));

    let top_bookmark = ctx.prefixed("f1top");
    let out = ctx.run_jjpr(&["submit", &top_bookmark]);
    assert!(
        out.status.success(),
        "{name}: jjpr submit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bottom = find_pr(&ctx, "f1bot");
    let top = find_pr(&ctx, "f1top");
    let sha_before = ctx.driver.request_head_sha(top);
    assert!(!sha_before.is_empty(), "{name}: descendant head sha present before merge");

    // Mark only the top draft (blocked) so jjpr merges the bottom and reconciles
    // the top without merging it.
    ctx.driver.make_draft(top);
    let out = ctx.run_jjpr(&[
        "merge",
        "--merge-method",
        method_flag,
        "--required-approvals",
        "0",
        "--no-ci-check",
        &top_bookmark,
    ]);
    assert!(
        out.status.success(),
        "{name}: jjpr merge failed ({method:?}): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        ctx.driver.request_state(bottom),
        "merged",
        "{name}: jjpr should have merged the bottom ({method:?})\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The draft top must survive for inspection; if a forge ignored the draft on
    // create, jjpr would have merged it too and this catches it clearly.
    assert_eq!(
        ctx.driver.request_state(top),
        "open",
        "{name}: draft descendant should stay open ({method:?})"
    );

    assert_eq!(
        ctx.driver.request_base(top),
        "main",
        "{name}: descendant base retargeted to trunk ({method:?})"
    );

    let sha_after = poll_head_sha(&ctx, top, &sha_before, expect_sha_changed);
    if expect_sha_changed {
        assert_ne!(
            sha_after, sha_before,
            "{name}: squash landing must rebase and force-push the descendant"
        );
    } else {
        assert_eq!(
            sha_after, sha_before,
            "{name}: merge-commit landing must skip the rebase and preserve the descendant SHA"
        );
    }
}
