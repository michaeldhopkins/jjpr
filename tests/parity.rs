//! Data-driven parity harness for jjpr submit / merge / watch.
//!
//! Each TOML file under `tests/parity_scenarios/` describes a stack to
//! build, optional setup steps (initial submit, external admin merges),
//! the jjpr subcommand to run, and the expected forge state afterward.
//!
//! Gated behind `JJPR_E2E=1` so unit-test runs stay fast and offline.
//!
//! Run with:
//!   JJPR_E2E=1 cargo test --test parity -- --nocapture
//!
//! Or a single scenario:
//!   JJPR_E2E=1 PARITY_SCENARIO=01-submit-creates-stack \
//!       cargo test --test parity -- --nocapture

mod parity_harness;

use std::path::PathBuf;

use parity_harness::assertions;
use parity_harness::context::{gh_available, jj_available, scenarios_dir, ParityContext};
use parity_harness::runner::{run_command, run_setup};
use parity_harness::scenario::Scenario;

#[test]
fn run_parity_scenarios() {
    if std::env::var("JJPR_E2E").is_err() {
        eprintln!("Skipping parity scenarios (set JJPR_E2E=1 to run)");
        return;
    }
    if !jj_available() {
        eprintln!("Skipping parity scenarios (jj not available)");
        return;
    }
    if !gh_available() {
        eprintln!("Skipping parity scenarios (gh not available)");
        return;
    }

    let only = std::env::var("PARITY_SCENARIO").ok();
    let files = collect_scenario_files();
    assert!(
        !files.is_empty(),
        "no parity scenarios found in {}",
        scenarios_dir().display()
    );

    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    let mut ran = 0;
    for path in files {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if let Some(filter) = &only
            && stem != *filter
        {
            continue;
        }
        ran += 1;

        eprintln!("\n=== parity scenario: {stem} ===");
        match run_one(&path) {
            Ok(()) => eprintln!("  ✓ {stem}"),
            Err(e) => {
                eprintln!("  ✗ {stem}: {e:#}");
                failures.push((stem, e));
            }
        }
    }

    assert!(ran > 0, "no scenarios ran (PARITY_SCENARIO filter matched nothing?)");
    if !failures.is_empty() {
        let summary: String = failures
            .iter()
            .map(|(name, err)| format!("\n  - {name}: {err:#}"))
            .collect();
        panic!("{} parity scenario(s) failed:{summary}", failures.len());
    }
}

fn run_one(path: &std::path::Path) -> anyhow::Result<()> {
    let toml_text = std::fs::read_to_string(path)?;
    let scenario: Scenario = toml::from_str(&toml_text)?;

    let ctx = ParityContext::new();
    ctx.build_stack(&scenario.stack);

    run_setup(&ctx, &scenario)?;
    let output = run_command(&ctx, &scenario);
    assertions::check(&ctx, &scenario, &output)
}

fn collect_scenario_files() -> Vec<PathBuf> {
    let dir = scenarios_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    paths
}
