//! `--verbose`: one stderr line for each jj call and forge request, with how long it took, and a
//! note whenever jjpr takes a slower path than it would like (GitHub's GraphQL falling back to
//! REST). It answers "what is jjpr waiting on", so it reports only what leaves the process.
//!
//! The switch is a process-wide flag rather than a parameter because the calls it reports sit
//! under every command, several layers down, and some run on worker threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Run `f`, and when verbose is on, print how long it took and what `describe` says about it.
/// `describe` sees the result, so a line can carry a status or say the call failed.
pub fn timed<T, E>(
    f: impl FnOnce() -> Result<T, E>,
    describe: impl FnOnce(&Result<T, E>) -> String,
) -> Result<T, E> {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let result = f();
    eprintln!("{}", line(start.elapsed(), &describe(&result)));
    result
}

/// Time one jj invocation. `args` is what jj was given.
pub fn jj<T, E>(args: &[&str], f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    program("jj", args, f)
}

/// Time one jj invocation run with `--ignore-working-copy`, which `args` leave out.
pub fn jj_ignoring_wc<T, E>(args: &[&str], f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let shown = [&["--ignore-working-copy"], args].concat();
    program("jj", &shown, f)
}

/// What vcs-runner's `jj_current_operation_id` runs, for its line.
pub const CURRENT_OP: &[&str] = &["op", "log", "-n1", "--no-graph", "-T", "id"];

/// vcs-runner's `jj_divergent_change_ids`, whose revset depends on the jj version.
pub const DIVERGENT: &[&str] = &["log", "-r", "<divergent>"];

/// Time one run of another program, such as `gh auth token` reading the GitHub token.
pub fn program<T, E>(name: &str, args: &[&str], f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    timed(f, |r| with_outcome(&command_line(name, args), r.is_err()))
}

/// Time one forge request, reporting the status it answered with. A request for `path` is shown
/// as that path (`repos/acme/app/pulls`): the API base is the same for every request in a run.
/// A URL that is not the path under some base, such as the next page a forge links to, is shown
/// whole.
pub fn http<B, E: std::fmt::Display>(
    method: &str,
    path: &str,
    url: &str,
    f: impl FnOnce() -> Result<ureq::http::Response<B>, E>,
) -> Result<ureq::http::Response<B>, E> {
    timed(f, |r| {
        let outcome = r.as_ref().map(|resp| resp.status().as_u16());
        request_line(
            method,
            shown(path, url),
            outcome.map_err(ToString::to_string),
        )
    })
}

fn shown<'a>(path: &'a str, url: &'a str) -> &'a str {
    let path = path.trim_start_matches('/');
    let under_a_base = url
        .strip_suffix(path)
        .is_some_and(|base| base.ends_with('/'));
    if !path.is_empty() && under_a_base {
        path
    } else {
        url
    }
}

fn request_line(method: &str, url: &str, outcome: Result<u16, String>) -> String {
    match outcome {
        Ok(status) => format!("{method} {url} {status}"),
        Err(e) => format!("{method} {url} failed: {e}"),
    }
}

/// A line with no timing, for a decision rather than a call.
pub fn note(message: impl FnOnce() -> String) {
    if enabled() {
        eprintln!("{}", note_line(&message()));
    }
}

/// GitHub's batched status query failed, and every PR's status is about to be read over REST,
/// several requests a PR. The reason is the part worth seeing: a SAML or permission denial
/// recurs on every run, a rate limit does not.
pub fn graphql_fallback(error: &anyhow::Error) {
    note(|| graphql_fallback_message(error));
}

/// Some PRs had more reviews or checks than one GraphQL page holds, and are read again over REST.
pub fn graphql_refill(prs: usize) {
    if let Some(message) = graphql_refill_message(prs) {
        note(|| message);
    }
}

fn graphql_fallback_message(error: &anyhow::Error) -> String {
    format!("GitHub GraphQL status query failed, so each PR's status comes from REST: {error:#}")
}

fn graphql_refill_message(prs: usize) -> Option<String> {
    let (count, verb) = match prs {
        0 => return None,
        1 => ("1 PR".to_string(), "has"),
        _ => (format!("{prs} PRs"), "have"),
    };
    Some(format!(
        "{count} {verb} over 100 reviews or checks, past one GraphQL page, so REST reads the rest"
    ))
}

fn with_outcome(what: &str, failed: bool) -> String {
    if failed {
        format!("{what} (failed)")
    } else {
        what.to_string()
    }
}

/// A program and its arguments, with a template (`-T`) shown as `<template>`: jjpr's jj
/// templates are a screenful of JSON each and say nothing about why a call was slow.
fn command_line(program: &str, args: &[&str]) -> String {
    let mut out = String::from(program);
    let mut after_template_flag = false;
    for arg in args {
        out.push(' ');
        if after_template_flag {
            out.push_str("<template>");
        } else if arg.is_empty() || arg.contains([' ', '"', '\'']) {
            out.push_str(&format!("'{arg}'"));
        } else {
            out.push_str(arg);
        }
        after_template_flag = matches!(*arg, "-T" | "--template");
    }
    out
}

/// Milliseconds, right-aligned so a column of calls reads at a glance.
fn line(elapsed: Duration, what: &str) -> String {
    format!("[{:>6}ms] {what}", elapsed.as_millis())
}

fn note_line(message: &str) -> String {
    format!("[  note  ] {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_right_aligns_the_milliseconds() {
        assert_eq!(
            line(Duration::from_millis(42), "jj log"),
            "[    42ms] jj log"
        );
        assert_eq!(
            line(Duration::from_millis(123_456), "jj log"),
            "[123456ms] jj log"
        );
    }

    #[test]
    fn a_note_lines_up_with_the_timed_lines() {
        let timed = line(Duration::from_millis(1), "x");
        let note = note_line("x");
        assert_eq!(timed.find(']'), note.find(']'));
        assert_eq!(note, "[  note  ] x");
    }

    #[test]
    fn command_line_hides_templates_and_quotes_spaced_arguments() {
        let args = [
            "log",
            "-r",
            "::@ ~ trunk()",
            "--no-graph",
            "-T",
            r#"json(self) ++ "\n""#,
            "--template",
            "x",
            "",
        ];
        assert_eq!(
            command_line("jj", &args),
            "jj log -r '::@ ~ trunk()' --no-graph -T <template> --template <template> ''"
        );
    }

    #[test]
    fn command_line_leaves_plain_arguments_alone() {
        assert_eq!(
            command_line("jj", &["git", "fetch", "--all-remotes"]),
            "jj git fetch --all-remotes"
        );
    }

    #[test]
    fn graphql_fallback_message_carries_the_whole_error_chain() {
        let error = anyhow::anyhow!("FORBIDDEN: Resource protected by organization SAML")
            .context("POST graphql");
        assert_eq!(
            graphql_fallback_message(&error),
            "GitHub GraphQL status query failed, so each PR's status comes from REST: \
             POST graphql: FORBIDDEN: Resource protected by organization SAML"
        );
    }

    #[test]
    fn graphql_refill_message_counts_prs() {
        assert_eq!(
            graphql_refill_message(0),
            None,
            "nothing refilled, nothing said"
        );
        assert_eq!(
            graphql_refill_message(1).as_deref(),
            Some(
                "1 PR has over 100 reviews or checks, past one GraphQL page, so REST reads the rest"
            )
        );
        assert_eq!(
            graphql_refill_message(3).as_deref(),
            Some(
                "3 PRs have over 100 reviews or checks, past one GraphQL page, so REST reads the rest"
            )
        );
    }

    #[test]
    fn request_line_shows_the_status_or_why_there_was_none() {
        let url = "repos/acme/app/pulls";
        assert_eq!(
            request_line("GET", url, Ok(200)),
            "GET repos/acme/app/pulls 200"
        );
        assert_eq!(
            request_line("POST", url, Err("connection refused".into())),
            "POST repos/acme/app/pulls failed: connection refused"
        );
    }

    #[test]
    fn shown_is_the_path_under_the_base_and_any_other_url_whole() {
        assert_eq!(
            shown(
                "repos/acme/app/pulls?state=open",
                "https://api.github.com/repos/acme/app/pulls?state=open"
            ),
            "repos/acme/app/pulls?state=open"
        );
        assert_eq!(shown("/user", "https://codeberg.org/api/v1/user"), "user");
        let next = "https://api.github.com/repositories/1/pulls?state=open&page=2";
        assert_eq!(
            shown("repos/acme/app/pulls?state=open", next),
            next,
            "a linked next page is not the path asked for"
        );
        assert_eq!(
            shown("user", "https://api.github.com/superuser"),
            "https://api.github.com/superuser",
            "a URL that merely ends with the path's text is not that path"
        );
        assert_eq!(
            shown("", "https://api.github.com/"),
            "https://api.github.com/"
        );
    }

    #[test]
    fn with_outcome_marks_only_a_failure() {
        assert_eq!(with_outcome("jj status", false), "jj status");
        assert_eq!(with_outcome("jj status", true), "jj status (failed)");
    }

    // One test owns the global flag, so parallel tests never see it flip under them.
    #[test]
    fn timed_passes_the_result_through_and_describes_only_when_enabled() {
        let mut described = false;
        let r: Result<u8, ()> = timed(
            || Ok(7),
            |_| {
                described = true;
                String::new()
            },
        );
        assert_eq!(r, Ok(7));
        assert!(!described, "off: describe must not run");

        set_enabled(true);
        assert!(enabled());
        let mut seen = None;
        let r: Result<u8, &str> = timed(
            || Err("boom"),
            |r| {
                seen = Some(*r);
                String::new()
            },
        );
        set_enabled(false);
        assert_eq!(r, Err("boom"));
        assert_eq!(seen, Some(Err("boom")), "on: describe sees the result");
        assert!(!enabled());
    }
}
