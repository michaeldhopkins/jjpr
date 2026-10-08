//! The dry run's account, and the notes a finished run ends with.

use std::io::Write;

use anyhow::Result;

use super::journal::Entry;
use super::plan::{Direction, Plan};
use super::{Options, explain, report};

pub(super) fn dry_run(
    out: &mut dyn Write,
    entry: &Entry,
    plan: &Plan,
    opts: Options,
    now: u64,
) -> Result<()> {
    writeln!(out, "{}", report::header(entry, opts.direction, true, now))?;
    if opts.force {
        for b in &plan.blockers {
            if let Some(line) = explain::overridden(b, entry) {
                writeln!(out, "{line}")?;
            }
        }
    }
    for step in &plan.steps {
        writeln!(
            out,
            "{}",
            report::step(step, entry, opts.direction, entry.forge)
        )?;
    }
    kept(out, plan, entry, opts.direction)?;
    if let Some(text) =
        explain::dry_run_blockers(&plan.blockers, entry, opts.direction, opts.force, now)
    {
        writeln!(out, "{text}")?;
    }
    writeln!(out, "{}", report::DRY_RUN_NOTE)?;
    Ok(())
}

pub(super) fn kept(
    out: &mut dyn Write,
    plan: &Plan,
    entry: &Entry,
    direction: Direction,
) -> Result<()> {
    if plan.kept.is_empty() {
        return Ok(());
    }
    writeln!(out, "{}", report::kept_heading(direction))?;
    for k in &plan.kept {
        writeln!(out, "{}", report::kept(k, entry.forge))?;
    }
    Ok(())
}
