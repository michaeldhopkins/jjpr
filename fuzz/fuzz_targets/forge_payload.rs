#![no_main]

//! SCHEMA-RESILIENCE target for forge JSON payloads.
//!
//! GitHub's stacked-PR API is a public *preview*: the shape can lose or rename a
//! field between one deploy and the next, with no version bump and no warning. jjpr's
//! answer was to mark every field on `Stack` / `StackPr` / `PrStackRef`
//! `#[serde(default)]` so a shrinking payload degrades instead of failing the whole
//! parse — and `jjpr merge` reads these payloads to decide whether it may merge, so a
//! parse failure there is not a cosmetic error, it blocks the command.
//!
//! That claim is currently defended by three hand-written "a partial payload still
//! parses" tests, which check three shapes out of 2^n. This asserts it over the whole
//! subset lattice: start from a payload captured live from the real API, delete an
//! arbitrary subset of its keys at any depth, and require it to still parse.
//!
//! Deliberately scoped to `Stack`. `PullRequest` is NOT all-defaulted — `number`,
//! `html_url` and `title` are required, and should be: a PR payload missing its
//! number is a broken response, not a preview-grade one. Asserting the same property
//! there would be asserting a design jjpr does not have.
//!
//! Also runs a plain never-panics pass, since arbitrary bytes off a network socket is
//! the other thing serde has to survive here.

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

use jjpr::forge::types::{IssueComment, PullRequest, Stack};

/// Captured live from `GET /repos/{owner}/{repo}/stacks/355` on a real
/// partially-merged stack (#353 merged, #354 open).
const STACK_JSON: &str = r#"{
  "id": 94565,
  "number": 355,
  "node_id": "PRS_kwDORPJ1DM4AAXFl",
  "base": { "ref": "main" },
  "open": true,
  "created_at": "2026-08-01T04:04:03Z",
  "pull_requests": [
    { "number": 353, "state": "closed", "draft": false,
      "merged_at": "2026-08-01T04:06:41Z",
      "head": { "ref": "fix0731-a", "sha": "f24f394275ab" },
      "base": { "ref": "main", "sha": "6a412bc6bfea" } },
    { "number": 354, "state": "open", "draft": false, "merged_at": null,
      "head": { "ref": "fix0731-b", "sha": "8bf5f5f34cfe" },
      "base": { "ref": "main", "sha": "8795b3b8df6d" } }
  ]
}"#;

/// Consumes the fuzz input one byte per decision, so libFuzzer's mutations map
/// directly onto "which keys went missing".
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    /// Out of input means "keep" — a short input then prunes a prefix of the tree
    /// rather than collapsing to the empty object every time.
    fn drop_it(&mut self) -> bool {
        let b = self.bytes.get(self.at).copied().unwrap_or(1);
        self.at += 1;
        b & 1 == 0
    }
}

fn prune(value: &mut Value, cursor: &mut Cursor) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if cursor.drop_it() {
                    map.remove(&key);
                } else if let Some(child) = map.get_mut(&key) {
                    prune(child, cursor);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                prune(item, cursor);
            }
        }
        _ => {}
    }
}

fuzz_target!(|data: &[u8]| {
    // 1. Never-panic: arbitrary bytes where a forge response was expected.
    let _ = serde_json::from_slice::<PullRequest>(data);
    let _ = serde_json::from_slice::<Stack>(data);
    let _ = serde_json::from_slice::<IssueComment>(data);

    // 2. Any subset of the real payload's keys may go missing.
    let mut value: Value = serde_json::from_str(STACK_JSON).expect("fixture is valid JSON");
    let mut cursor = Cursor { bytes: data, at: 0 };
    prune(&mut value, &mut cursor);

    let pruned = value.to_string();
    let stack: Stack = serde_json::from_str(&pruned).unwrap_or_else(|e| {
        panic!("a shrinking stack payload failed to parse: {e}\npayload: {pruned}")
    });

    // The helpers the merge pre-flight calls must survive a degraded payload too —
    // a parse that succeeds but leaves a helper panicking has moved the failure,
    // not removed it.
    for pr in &stack.pull_requests {
        let _ = pr.would_block_merge();
    }
    for target in [0u64, 353, 354, u64::MAX] {
        if let Some(members) = stack.members_landed_by(target) {
            assert!(
                members.len() <= stack.pull_requests.len(),
                "members_landed_by returned more members than the stack has"
            );
        }
        let _ = stack.blocker_for(target);
    }
});
