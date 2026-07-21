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

use forge_e2e_harness::{configured_drivers, ForgeE2eContext, MergeMethod, OWNER, REPO};

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
