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

use forge_e2e_harness::{configured_drivers, ForgeE2eContext, MergeMethod};

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
