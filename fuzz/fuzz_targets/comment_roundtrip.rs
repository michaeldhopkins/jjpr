#![no_main]

//! ROUNDTRIP target: the stack-nav comment must always be readable back.
//!
//! The nav comment is jjpr's only durable memory of a stack's shape. jjpr writes a
//! base64 `<!--- JJPR_DATA: … --->` line into a PR comment and reads it back on the
//! next run to know which PRs were in the stack and which are fossils. If a body
//! jjpr generated cannot be parsed by jjpr, the stack silently loses its history:
//! fossils vanish, and the comment is rebuilt from scratch as if the earlier PRs
//! never existed. No test catches that by example, because the inputs that break it
//! are exactly the ones nobody writes by hand — a bookmark name or PR URL carrying
//! a newline, a `--->`, or a base64-looking run.
//!
//! Structure-aware (`arbitrary`-derived) rather than byte-mutated: the input is a
//! *stack*, not a string, so the mutator spends its budget on entry shapes and
//! adversarial names instead of on rediscovering that random bytes are not a comment.
//!
//! Two properties:
//!
//! 1. ALWAYS PARSEABLE. Whatever entries go in, `parse_comment_data` returns `Some`.
//! 2. FAITHFUL, AND FIRST. The recovered items equal the ones jjpr meant to persist,
//!    in order. This is also the anti-injection assertion: `parse_comment_data`
//!    returns the FIRST `JJPR_DATA` line in the body, and the entries jjpr renders
//!    below it are attacker-influenced text (a bookmark name comes from a remote).
//!    Today the data line is written above them, so a forged one can never be found
//!    first — this pins that ordering, because moving the data line to the footer
//!    would quietly turn a rendered bookmark name into a way to substitute the whole
//!    stack's metadata.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use jjpr::forge::comment::{generate_comment_body, parse_comment_data, StackEntry};

#[derive(Arbitrary, Debug)]
struct Entry {
    bookmark_name: String,
    pr_url: Option<String>,
    pr_number: Option<u64>,
    is_current: bool,
    is_merged: bool,
    closed_at: Option<String>,
}

impl From<Entry> for StackEntry {
    fn from(e: Entry) -> Self {
        StackEntry {
            bookmark_name: e.bookmark_name,
            pr_url: e.pr_url,
            pr_number: e.pr_number,
            is_current: e.is_current,
            is_merged: e.is_merged,
            closed_at: e.closed_at,
        }
    }
}

#[derive(Arbitrary, Debug)]
struct Input {
    live: Vec<Entry>,
    fossils: Vec<Entry>,
    /// Plant a forged `JJPR_DATA` line inside a rendered bookmark name.
    plant_forgery: bool,
}

/// A syntactically perfect `JJPR_DATA` line describing a stack that is not ours.
///
/// Built by asking jjpr's own generator for a comment and lifting its data line
/// out, so the forgery is byte-for-byte the real thing rather than an
/// approximation — no base64 dependency and no chance of testing against a
/// payload the parser would have rejected anyway.
///
/// This is what makes the ordering assertion non-vacuous. Mutation-tested: with
/// the data line moved below the rendered entries, 90s of fuzzing found nothing
/// at all, because `arbitrary` will not synthesise valid base64 of valid JSON by
/// chance. Planting the forgery turns that from a property the fuzzer cannot
/// reach into one it checks on every input.
fn forged_data_line() -> String {
    let decoy = [StackEntry {
        bookmark_name: "ATTACKER-CONTROLLED".to_string(),
        pr_url: Some("https://example.invalid/pull/999999".to_string()),
        pr_number: Some(999_999),
        is_current: false,
        is_merged: false,
        closed_at: None,
    }];
    generate_comment_body(&decoy, &[])
        .lines()
        .find(|l| l.trim_start().starts_with("<!--- JJPR_DATA: "))
        .expect("the generator must emit a data line")
        .trim()
        .to_string()
}

/// What `generate_comment_body` persists: live then fossils, keeping only entries
/// that have BOTH a URL and a number. Mirrors the producer deliberately — the point
/// is to detect the producer and consumer drifting apart.
fn expected(live: &[StackEntry], fossils: &[StackEntry]) -> Vec<(String, String, u64)> {
    live.iter()
        .chain(fossils.iter())
        .filter_map(|e| match (&e.pr_url, e.pr_number) {
            (Some(url), Some(number)) => {
                Some((e.bookmark_name.clone(), url.clone(), number))
            }
            _ => None,
        })
        .collect()
}

fuzz_target!(|input: Input| {
    // A runaway stack is a memory-size finding, not a correctness one, and the
    // comment is capped by GitHub's body limit long before this.
    if input.live.len() + input.fossils.len() > 64 {
        return;
    }

    let mut live: Vec<StackEntry> = input.live.into_iter().map(Into::into).collect();
    let fossils: Vec<StackEntry> = input.fossils.into_iter().map(Into::into).collect();

    // Splice a valid-but-foreign data line into text the comment will RENDER.
    // A bookmark name can come from a remote, so this is attacker-influenced
    // input, not a hypothetical. If the real data line is ever emitted after the
    // entries, `parse_comment_data` — which returns the FIRST match — reads this
    // one instead, and the assertion below reports the whole stack's metadata
    // having been substituted.
    if input.plant_forgery && let Some(first) = live.first_mut() {
        first.bookmark_name = format!("{}\n{}\n", first.bookmark_name, forged_data_line());
    }

    let body = generate_comment_body(&live, &fossils);

    let data = parse_comment_data(&body).unwrap_or_else(|| {
        panic!("jjpr generated a comment body it cannot parse back:\n{body}")
    });

    let got: Vec<(String, String, u64)> = data
        .stack
        .iter()
        .map(|i| (i.bookmark_name.clone(), i.pr_url.clone(), i.pr_number))
        .collect();

    assert_eq!(
        got,
        expected(&live, &fossils),
        "roundtrip did not recover the persisted stack (a forged JJPR_DATA line \
         found before the real one would look exactly like this)\nbody:\n{body}"
    );
});
