use anyhow::{anyhow, bail, Result};

use super::context::{fetch_pr_detail, find_pr_by_head, list_comments, ParityContext};
use super::runner::RunOutput;
use super::scenario::{
    CommentExpectation, ExitStatus, Expectations, PrExpectation, PrStateExpect, Scenario,
};

/// Run every assertion against the captured output and live forge state.
/// Returns Ok(()) if all pass, Err with the first failing assertion otherwise.
pub fn check(
    ctx: &ParityContext,
    scenario: &Scenario,
    output: &RunOutput,
) -> Result<()> {
    check_exit(&scenario.expect, output)?;
    check_stderr(ctx, &scenario.expect, output)?;
    for pr_expect in &scenario.expect.prs {
        check_pr(ctx, pr_expect)?;
    }
    for comment_expect in &scenario.expect.comments {
        check_comment(ctx, comment_expect)?;
    }
    Ok(())
}

fn check_exit(expect: &Expectations, output: &RunOutput) -> Result<()> {
    let succeeded = output.status.success();
    match expect.exit_status {
        ExitStatus::Success if !succeeded => bail!(
            "expected success, got exit {:?}\nstdout: {}\nstderr: {}",
            output.status.code(), output.stdout, output.stderr
        ),
        ExitStatus::Failure if succeeded => bail!(
            "expected failure, got success\nstdout: {}\nstderr: {}",
            output.stdout, output.stderr
        ),
        _ => Ok(()),
    }
}

fn check_stderr(ctx: &ParityContext, expect: &Expectations, output: &RunOutput) -> Result<()> {
    for needle in &expect.stderr_contains {
        let n = resolve_bookmark_substring(ctx, needle);
        if !output.stderr.contains(&n) {
            bail!(
                "expected stderr to contain '{n}'\nstderr was:\n{}",
                output.stderr
            );
        }
    }
    for forbidden in &expect.stderr_not_contains {
        let f = resolve_bookmark_substring(ctx, forbidden);
        if output.stderr.contains(&f) {
            bail!(
                "expected stderr NOT to contain '{f}'\nstderr was:\n{}",
                output.stderr
            );
        }
    }
    for needle in &expect.stdout_contains {
        let n = resolve_bookmark_substring(ctx, needle);
        if !output.stdout.contains(&n) {
            bail!(
                "expected stdout to contain '{n}'\nstdout was:\n{}",
                output.stdout
            );
        }
    }
    for forbidden in &expect.stdout_not_contains {
        let f = resolve_bookmark_substring(ctx, forbidden);
        if output.stdout.contains(&f) {
            bail!(
                "expected stdout NOT to contain '{f}'\nstdout was:\n{}",
                output.stdout
            );
        }
    }
    Ok(())
}

fn check_pr(ctx: &ParityContext, expect: &PrExpectation) -> Result<()> {
    let head = ctx.prefixed(&expect.bookmark);

    // `find_pr_by_head` returns the most recent PR for that head ref,
    // including merged or closed ones. That's what we want for state checks.
    let pr_summary = find_pr_by_head(&head);

    if let Some(state_expect) = expect.state {
        match (state_expect, pr_summary.as_ref()) {
            (PrStateExpect::Absent, None) => {}
            (PrStateExpect::Absent, Some(pr)) => bail!(
                "expected no PR for bookmark '{}', found #{}",
                expect.bookmark,
                pr["number"].as_u64().unwrap_or(0)
            ),
            (_, None) => bail!(
                "expected PR for bookmark '{}' with state {:?}, found nothing",
                expect.bookmark, state_expect
            ),
            (want, Some(pr)) => {
                let actual = pr["state"].as_str().unwrap_or("");
                let actual_norm = actual.to_ascii_lowercase();
                let merged = matches!(actual_norm.as_str(), "merged");
                let closed = matches!(actual_norm.as_str(), "closed");
                let open = matches!(actual_norm.as_str(), "open");
                let matches = match want {
                    PrStateExpect::Open => open,
                    PrStateExpect::Merged => merged,
                    PrStateExpect::Closed => closed || merged,
                    PrStateExpect::Absent => false,
                };
                if !matches {
                    bail!(
                        "PR for '{}' had state '{}', expected {:?}",
                        expect.bookmark, actual, want
                    );
                }
            }
        }
    }

    let pr = match pr_summary {
        Some(pr) => pr,
        None => return Ok(()),
    };
    let number = pr["number"]
        .as_u64()
        .ok_or_else(|| anyhow!("PR for '{}' has no number", expect.bookmark))?;

    if let Some(want_base) = &expect.base {
        let actual_base = pr["baseRefName"].as_str().unwrap_or("");
        // Resolve "main" / "master" literally, but treat any other base name
        // as a stack-relative bookmark and apply the prefix.
        let want_resolved = if want_base == "main" || want_base == "master" {
            want_base.clone()
        } else {
            ctx.prefixed(want_base)
        };
        if actual_base != want_resolved {
            bail!(
                "PR #{number} for '{}' had base '{actual_base}', expected '{want_resolved}'",
                expect.bookmark
            );
        }
    }

    if expect.commit_count_max.is_some() || expect.diff_lines_max.is_some() {
        let detail = fetch_pr_detail(number)
            .ok_or_else(|| anyhow!("could not fetch detail for PR #{number}"))?;

        if let Some(max) = expect.commit_count_max {
            let actual = detail["commits"]
                .as_array()
                .map(|a| a.len() as u64)
                .unwrap_or(0);
            if actual > max {
                bail!(
                    "PR #{number} for '{}' had {actual} commits, expected ≤ {max} \
                     (likely a bloated-diff regression — local rebase did not run)",
                    expect.bookmark
                );
            }
        }

        if let Some(max) = expect.diff_lines_max {
            let additions = detail["additions"].as_u64().unwrap_or(0);
            let deletions = detail["deletions"].as_u64().unwrap_or(0);
            let total = additions + deletions;
            if total > max {
                bail!(
                    "PR #{number} for '{}' had {total} changed lines, expected ≤ {max} \
                     (likely a bloated-diff regression)",
                    expect.bookmark
                );
            }
        }
    }

    Ok(())
}

fn check_comment(ctx: &ParityContext, expect: &CommentExpectation) -> Result<()> {
    let head = ctx.prefixed(&expect.bookmark);
    let pr = find_pr_by_head(&head)
        .ok_or_else(|| anyhow!("no PR found for bookmark '{}'", expect.bookmark))?;
    let number = pr["number"]
        .as_u64()
        .ok_or_else(|| anyhow!("PR for '{}' has no number", expect.bookmark))?;

    let comments = list_comments(number);
    let stack_comment = comments.iter().find(|c| {
        c["body"]
            .as_str()
            .unwrap_or("")
            .contains("<!-- jjpr:stack-info -->")
    });

    let body = match stack_comment {
        Some(c) => c["body"].as_str().unwrap_or("").to_string(),
        None => {
            if expect.contains.is_empty() {
                return Ok(());
            }
            bail!(
                "PR #{number} for '{}' has no jjpr stack-info comment",
                expect.bookmark
            );
        }
    };

    for needle in &expect.contains {
        // Bookmark names referenced inside scenarios are scenario-relative —
        // resolve them through the prefix so assertions match the live PR.
        let resolved = resolve_bookmark_substring(ctx, needle);
        if !body.contains(&resolved) {
            bail!(
                "stack comment on PR #{number} ('{}') missing '{resolved}'\nbody:\n{body}",
                expect.bookmark
            );
        }
    }
    for forbidden in &expect.not_contains {
        let resolved = resolve_bookmark_substring(ctx, forbidden);
        if body.contains(&resolved) {
            bail!(
                "stack comment on PR #{number} ('{}') unexpectedly contains '{resolved}'\nbody:\n{body}",
                expect.bookmark
            );
        }
    }
    Ok(())
}

/// Scenarios reference bookmarks by their unprefixed name. Substrings of the
/// form `{{bookmark:NAME}}` get rewritten to the live prefixed name so
/// expectations stay readable.
fn resolve_bookmark_substring(ctx: &ParityContext, raw: &str) -> String {
    let mut out = raw.to_string();
    while let Some(start) = out.find("{{bookmark:") {
        let rest = &out[start + "{{bookmark:".len()..];
        let Some(end) = rest.find("}}") else { break };
        let name = &rest[..end];
        let replacement = ctx.prefixed(name);
        let full_token_end = start + "{{bookmark:".len() + end + "}}".len();
        out.replace_range(start..full_token_end, &replacement);
    }
    out
}
