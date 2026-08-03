/// jj template strings for structured JSON output, and parsing logic.
use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::types::{Bookmark, LogEntry};

/// Template for `jj bookmark list` that produces line-delimited JSON.
/// Note: jj's escape_json() includes surrounding quotes, so array elements
/// use escape_json() directly with comma joins (no extra quote wrapping).
pub const BOOKMARK_TEMPLATE: &str = concat!(
    r#"'{"name":' ++ name.escape_json()"#,
    r#" ++ ',"commitId":' ++ normal_target.commit_id().short().escape_json()"#,
    r#" ++ ',"changeId":' ++ normal_target.change_id().short().escape_json()"#,
    r#" ++ ',"localBookmarks":[' ++ normal_target.local_bookmarks().map(|b| b.name().escape_json()).join(',') ++ ']'"#,
    r#" ++ ',"remoteBookmarks":[' ++ normal_target.remote_bookmarks().map(|b| stringify(b.name() ++ "@" ++ b.remote()).escape_json()).join(',') ++ ']'"#,
    r#" ++ '}' ++ "\n""#,
);

/// Template for `jj log` that produces line-delimited JSON entries.
/// Note: jj's escape_json() includes surrounding quotes, so array elements
/// use escape_json() directly with comma joins (no extra quote wrapping).
pub const LOG_TEMPLATE: &str = concat!(
    r#"'{"commitId":' ++ commit_id.short().escape_json()"#,
    r#" ++ ',"changeId":' ++ change_id.short().escape_json()"#,
    r#" ++ ',"authorName":' ++ author.name().escape_json()"#,
    r#" ++ ',"authorEmail":' ++ stringify(author.email()).escape_json()"#,
    r#" ++ ',"description":' ++ description.escape_json()"#,
    r#" ++ ',"descriptionFirstLine":' ++ description.first_line().escape_json()"#,
    r#" ++ ',"parents":[' ++ parents.map(|p| p.commit_id().short().escape_json()).join(',') ++ ']'"#,
    r#" ++ ',"localBookmarks":[' ++ local_bookmarks.map(|b| b.name().escape_json()).join(',') ++ ']'"#,
    r#" ++ ',"remoteBookmarks":[' ++ remote_bookmarks.map(|b| stringify(b.name() ++ "@" ++ b.remote()).escape_json()).join(',') ++ ']'"#,
    r#" ++ ',"isWorkingCopy":' ++ if(current_working_copy, '"true"', '"false"')"#,
    r#" ++ ',"conflict":' ++ if(conflict, '"true"', '"false"')"#,
    r#" ++ ',"empty":' ++ if(empty, '"true"', '"false"')"#,
    r#" ++ '}' ++ "\n""#,
);

/// Best-effort name extraction from malformed bookmark JSON.
///
/// The `"name"` field is always a valid quoted string (it's the bookmark name,
/// not commit-dependent), so we can extract it even when the rest is broken.
fn extract_name_from_malformed_json(line: &str) -> Option<String> {
    // Format is always {"name":"<value>",...} — find the quoted value after "name":
    let after_key = line.split(r#""name":"#).nth(1)?;
    // after_key starts with `"value",...` — strip the opening quote, then find the closing one
    let after_quote = after_key.strip_prefix('"')?;
    let end = after_quote.find('"')?;
    Some(after_quote[..end].to_string())
}

/// Raw bookmark JSON as returned by jj's bookmark template.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBookmark {
    name: String,
    commit_id: String,
    change_id: String,
    local_bookmarks: Vec<String>,
    remote_bookmarks: Vec<String>,
}

/// Parse `jj bookmark list` output into `Bookmark` values.
///
/// When a bookmark diverges from its remote, jj returns two entries: one for
/// the local target and one for the remote target. We filter out remote-only
/// entries (empty `localBookmarks`) to avoid the remote entry overwriting the
/// local one in downstream HashMaps.
///
/// Returns `(bookmarks, warnings)` where `warnings` is the list of bookmark
/// names whose every entry was unparseable. Names with at least one good
/// entry (e.g., a healthy local target plus a stale `@origin` target) must
/// not appear in `warnings` — the bookmark is being kept, not skipped.
pub fn parse_bookmark_output(output: &str) -> Result<(Vec<Bookmark>, Vec<String>)> {
    let mut bookmarks = Vec::new();
    let mut parsed_names: HashSet<String> = HashSet::new();
    let mut malformed_names: Vec<String> = Vec::new();
    let mut seen_unknown_malformed = false;

    // First pass: parse every line. Track which names had at least one
    // good entry. Divergent bookmarks emit one line per target (local +
    // @origin), so a healthy local entry can coexist with a stale @origin
    // entry that fails to parse — in that case we keep the bookmark and
    // suppress the warning.
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        let raw: RawBookmark = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => {
                match extract_name_from_malformed_json(line) {
                    Some(name) => malformed_names.push(name),
                    None => seen_unknown_malformed = true,
                }
                continue;
            }
        };

        // A line can be valid JSON and still carry no usable identity — an empty
        // `changeId` or `commitId`. Serde has nothing to object to, so without
        // this the bookmark reaches the change graph with an empty change id,
        // and from there `rebase_root` hands "" to revset construction
        // (`change_id()::name` — a syntax error) and to `jj rebase -s ''`. The
        // user would see an opaque jj parse error instead of the skip-and-warn
        // this function already does for a line it cannot read.
        //
        // Not producible by jj today, whose templates always emit both. It is a
        // trust-boundary guard: jjpr parses another program's output across
        // version upgrades, and this costs one comparison. Found by the
        // `graph_invariants` fuzz target.
        if raw.change_id.is_empty() || raw.commit_id.is_empty() {
            if raw.name.is_empty() {
                seen_unknown_malformed = true;
            } else {
                malformed_names.push(raw.name);
            }
            continue;
        }

        let non_git_remotes: Vec<&String> = raw
            .remote_bookmarks
            .iter()
            .filter(|rb| !rb.is_empty() && !rb.ends_with("@git"))
            .collect();

        let has_remote = !non_git_remotes.is_empty();

        // Synced if a remote bookmark with the same name exists (excluding @git).
        // For the local target, @origin only appears when both point to the same commit.
        let is_synced = non_git_remotes
            .iter()
            .any(|rb| rb.starts_with(&format!("{}@", raw.name)));

        // Track every parseable line — including remote-only ones — so a
        // bookmark whose @origin target parses but local target doesn't
        // (or vice versa) is still recognized as having a good entry.
        parsed_names.insert(raw.name.clone());

        // Skip remote-only entries from the returned bookmarks list.
        if raw.local_bookmarks.is_empty() {
            continue;
        }

        bookmarks.push(Bookmark {
            name: raw.name,
            commit_id: raw.commit_id,
            change_id: raw.change_id,
            has_remote,
            is_synced,
        });
    }

    // Only warn for names whose every entry was malformed. Dedupe so a
    // bookmark with multiple bad entries doesn't repeat.
    let mut seen_warning: HashSet<String> = HashSet::new();
    let mut warnings: Vec<String> = Vec::new();
    for name in malformed_names {
        if parsed_names.contains(&name) {
            continue;
        }
        if seen_warning.insert(name.clone()) {
            warnings.push(name);
        }
    }

    if seen_unknown_malformed {
        eprintln!("  Warning: skipping unparseable bookmark entry");
    }

    Ok((bookmarks, warnings))
}

/// Raw log entry JSON as returned by jj's log template.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLogEntry {
    commit_id: String,
    change_id: String,
    author_name: String,
    author_email: String,
    description: String,
    description_first_line: String,
    parents: Vec<String>,
    local_bookmarks: Vec<String>,
    remote_bookmarks: Vec<String>,
    is_working_copy: String,
    conflict: String,
    empty: String,
}

/// Parse `jj log` output into `LogEntry` values.
pub fn parse_log_output(output: &str) -> Result<Vec<LogEntry>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let raw: RawLogEntry = serde_json::from_str(line)
                .with_context(|| format!("failed to parse log JSON: {line}"))?;

            Ok(LogEntry {
                commit_id: raw.commit_id,
                change_id: raw.change_id,
                author_name: raw.author_name,
                author_email: raw.author_email,
                description: raw.description,
                description_first_line: raw.description_first_line,
                parents: raw.parents.into_iter().filter(|p| !p.is_empty()).collect(),
                local_bookmarks: raw
                    .local_bookmarks
                    .into_iter()
                    .filter(|b| !b.is_empty())
                    .collect(),
                remote_bookmarks: raw
                    .remote_bookmarks
                    .into_iter()
                    .filter(|b| !b.is_empty())
                    .collect(),
                is_working_copy: raw.is_working_copy == "true",
                conflict: raw.conflict == "true",
                empty: raw.empty == "true",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bookmark_no_remote() {
        let output = r#"{"name":"feature","commitId":"abc123","changeId":"xyz789","localBookmarks":["feature"],"remoteBookmarks":[]}"#;
        let (bookmarks, _warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "feature");
        assert_eq!(bookmarks[0].commit_id, "abc123");
        assert!(!bookmarks[0].has_remote);
        assert!(!bookmarks[0].is_synced);
    }

    // Valid JSON, no usable identity. Serde accepts it, so the malformed-line
    // branch never sees it, and the bookmark used to reach the change graph with
    // an empty change id — which `rebase_root` then hands to revset construction
    // as `change_id()::name`, a syntax error, and to `jj rebase -s ''`. Skipping
    // and warning is what this function already does for a line it cannot read.
    // Found by the `graph_invariants` fuzz target.
    #[test]
    fn a_bookmark_line_with_no_identity_is_treated_as_malformed() {
        for line in [
            r#"{"name":"feat","commitId":"c0","changeId":"","localBookmarks":["feat"],"remoteBookmarks":[]}"#,
            r#"{"name":"feat","commitId":"","changeId":"ch0","localBookmarks":["feat"],"remoteBookmarks":[]}"#,
        ] {
            let (bookmarks, warnings) = parse_bookmark_output(line).unwrap();
            assert!(
                bookmarks.is_empty(),
                "a bookmark with no usable identity must not reach the graph: {line}"
            );
            assert_eq!(
                warnings,
                vec!["feat".to_string()],
                "and the user is told: {line}"
            );
        }
    }

    // The counterpart: a healthy entry for the same name suppresses the warning,
    // exactly as it does for an unparseable @origin target alongside a good local
    // one. The empty-identity check must not break that.
    #[test]
    fn a_good_entry_still_suppresses_the_no_identity_warning() {
        let output = concat!(
            r#"{"name":"feat","commitId":"c0","changeId":"","localBookmarks":[],"remoteBookmarks":["feat@origin"]}"#,
            "\n",
            r#"{"name":"feat","commitId":"c1","changeId":"ch1","localBookmarks":["feat"],"remoteBookmarks":[]}"#,
        );
        let (bookmarks, warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1, "the healthy entry is kept");
        assert_eq!(bookmarks[0].change_id, "ch1");
        assert!(
            warnings.is_empty(),
            "no warning when a good entry exists: {warnings:?}"
        );
    }

    #[test]
    fn test_parse_bookmark_with_synced_remote() {
        let output = r#"{"name":"feature","commitId":"abc123","changeId":"xyz789","localBookmarks":["feature"],"remoteBookmarks":["feature@origin"]}"#;
        let (bookmarks, _warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert!(bookmarks[0].has_remote);
        assert!(bookmarks[0].is_synced);
    }

    #[test]
    fn test_parse_bookmark_with_git_remote_only() {
        let output = r#"{"name":"feature","commitId":"abc123","changeId":"xyz789","localBookmarks":["feature"],"remoteBookmarks":["feature@git"]}"#;
        let (bookmarks, _warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert!(!bookmarks[0].has_remote, "@git remotes should be excluded");
        assert!(!bookmarks[0].is_synced);
    }

    #[test]
    fn test_parse_bookmark_multiple() {
        let output = concat!(
            r#"{"name":"auth","commitId":"aaa","changeId":"111","localBookmarks":["auth"],"remoteBookmarks":["auth@origin"]}"#,
            "\n",
            r#"{"name":"profile","commitId":"bbb","changeId":"222","localBookmarks":["profile"],"remoteBookmarks":[]}"#,
            "\n",
        );
        let (bookmarks, _warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[0].name, "auth");
        assert!(bookmarks[0].is_synced);
        assert_eq!(bookmarks[1].name, "profile");
        assert!(!bookmarks[1].has_remote);
    }

    #[test]
    fn test_parse_bookmark_divergent_filters_remote_entry() {
        // When a bookmark diverges, jj returns two entries: local and remote target.
        // We should keep only the local entry.
        let output = concat!(
            r#"{"name":"feature","commitId":"new111","changeId":"ch1","localBookmarks":["feature"],"remoteBookmarks":["feature@git"]}"#,
            "\n",
            r#"{"name":"feature","commitId":"old222","changeId":"ch1","localBookmarks":[],"remoteBookmarks":["feature@origin"]}"#,
            "\n",
        );
        let (bookmarks, _warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1, "should filter out remote-only entry");
        assert_eq!(bookmarks[0].commit_id, "new111", "should keep local target");
        assert!(!bookmarks[0].is_synced, "divergent bookmark is not synced");
        assert!(!bookmarks[0].has_remote, "local entry lacks @origin");
    }

    #[test]
    fn test_parse_bookmark_conflicted_skipped() {
        // When a bookmark points to a missing commit (e.g., after squash merge),
        // jj outputs <Error: No Commit available> which isn't valid JSON values.
        // These should be skipped, not cause a hard error.
        let output = concat!(
            r#"{"name":"feat/stale","commitId":<Error: No Commit available>,"changeId":<Error: No Commit available>,"localBookmarks":[<Error: No Commit available>],"remoteBookmarks":[<Error: No Commit available>]}"#,
            "\n",
            r#"{"name":"feat/good","commitId":"abc123","changeId":"xyz789","localBookmarks":["feat/good"],"remoteBookmarks":["feat/good@origin"]}"#,
            "\n",
        );
        let (bookmarks, warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1, "should skip unparseable bookmark");
        assert_eq!(bookmarks[0].name, "feat/good");
        assert_eq!(
            warnings,
            vec!["feat/stale".to_string()],
            "fully-unparseable bookmark must produce a warning"
        );
    }

    /// Regression test for false-positive "skipping" warning observed on
    /// MerchantsBonding/beancounter PR #1875: jjpr warned that
    /// `feat/mbc-users-cache-table` was being skipped, then the same submit
    /// successfully pushed it. Cause: the bookmark had a healthy local
    /// target plus a stale `@origin` target whose commit had been abandoned,
    /// so the @origin line failed to parse and tripped the warning even
    /// though the local entry was kept and used.
    #[test]
    fn test_parse_bookmark_no_warning_when_local_entry_parses() {
        // Healthy local entry first, then a malformed @origin entry for the
        // same bookmark name (commit was abandoned remotely or after a
        // local rewrite).
        let output = concat!(
            r#"{"name":"feature","commitId":"good_local","changeId":"ch1","localBookmarks":["feature"],"remoteBookmarks":["feature@git"]}"#,
            "\n",
            r#"{"name":"feature","commitId":<Error: No Commit available>,"changeId":<Error: No Commit available>,"localBookmarks":[],"remoteBookmarks":["feature@origin"]}"#,
            "\n",
        );
        let (bookmarks, warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1, "local target should be returned");
        assert_eq!(bookmarks[0].name, "feature");
        assert_eq!(bookmarks[0].commit_id, "good_local");
        assert!(
            warnings.is_empty(),
            "no warning when bookmark has a healthy entry; got {warnings:?}"
        );
    }

    /// Same as above, but order reversed: malformed line first, healthy
    /// entry second. The fix must look at the whole batch, not just the
    /// first occurrence per name.
    #[test]
    fn test_parse_bookmark_no_warning_when_good_entry_comes_after_bad() {
        let output = concat!(
            r#"{"name":"feature","commitId":<Error: No Commit available>,"changeId":<Error: No Commit available>,"localBookmarks":[],"remoteBookmarks":["feature@origin"]}"#,
            "\n",
            r#"{"name":"feature","commitId":"good_local","changeId":"ch1","localBookmarks":["feature"],"remoteBookmarks":["feature@git"]}"#,
            "\n",
        );
        let (bookmarks, warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1, "local target should still be returned");
        assert_eq!(bookmarks[0].commit_id, "good_local");
        assert!(
            warnings.is_empty(),
            "ordering must not affect warning suppression; got {warnings:?}"
        );
    }

    /// Mixed scenario: one fully-broken bookmark and one bookmark with a
    /// healthy local entry plus a stale @origin entry. Only the
    /// fully-broken one should produce a warning.
    #[test]
    fn test_parse_bookmark_warns_only_for_fully_unparseable() {
        let output = concat!(
            r#"{"name":"feat/stale","commitId":<Error: No Commit available>,"changeId":<Error: No Commit available>,"localBookmarks":[<Error: No Commit available>],"remoteBookmarks":[<Error: No Commit available>]}"#,
            "\n",
            r#"{"name":"feat/healthy","commitId":"good","changeId":"ch","localBookmarks":["feat/healthy"],"remoteBookmarks":["feat/healthy@git"]}"#,
            "\n",
            r#"{"name":"feat/healthy","commitId":<Error: No Commit available>,"changeId":<Error: No Commit available>,"localBookmarks":[],"remoteBookmarks":["feat/healthy@origin"]}"#,
            "\n",
        );
        let (bookmarks, warnings) = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "feat/healthy");
        assert_eq!(
            warnings,
            vec!["feat/stale".to_string()],
            "warning list must contain only the fully-unparseable bookmark"
        );
    }

    /// A bookmark whose every entry is malformed must produce exactly one
    /// warning, not one per malformed line.
    #[test]
    fn test_parse_bookmark_dedupes_warning_for_multiple_bad_entries() {
        let output = concat!(
            r#"{"name":"feat/dead","commitId":<Error: No Commit available>,"changeId":<Error: No Commit available>,"localBookmarks":["feat/dead"],"remoteBookmarks":[]}"#,
            "\n",
            r#"{"name":"feat/dead","commitId":<Error: No Commit available>,"changeId":<Error: No Commit available>,"localBookmarks":[],"remoteBookmarks":["feat/dead@origin"]}"#,
            "\n",
        );
        let (bookmarks, warnings) = parse_bookmark_output(output).unwrap();
        assert!(bookmarks.is_empty());
        assert_eq!(warnings, vec!["feat/dead".to_string()]);
    }

    #[test]
    fn test_extract_name_from_malformed_json() {
        let line = r#"{"name":"feat/stale","commitId":<Error: No Commit available>}"#;
        assert_eq!(
            extract_name_from_malformed_json(line),
            Some("feat/stale".to_string())
        );

        assert_eq!(extract_name_from_malformed_json("garbage"), None);
    }

    #[test]
    fn test_parse_bookmark_empty_output() {
        let (bookmarks, warnings) = parse_bookmark_output("").unwrap();
        assert!(bookmarks.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_parse_log_entry() {
        let output = r#"{"commitId":"abc123","changeId":"xyz789","authorName":"Alice","authorEmail":"alice@example.com","description":"Add feature\n\nDetailed description","descriptionFirstLine":"Add feature","parents":["def456"],"localBookmarks":["feature"],"remoteBookmarks":[],"isWorkingCopy":"false","conflict":"false","empty":"false"}"#;
        let entries = parse_log_output(output).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].commit_id, "abc123");
        assert_eq!(entries[0].description_first_line, "Add feature");
        assert_eq!(entries[0].parents, vec!["def456"]);
        assert!(!entries[0].is_working_copy);
        assert!(!entries[0].conflict);
        assert!(!entries[0].empty);
    }

    #[test]
    fn test_parse_log_empty_commit() {
        let output = r#"{"commitId":"abc","changeId":"xyz","authorName":"A","authorEmail":"a@b","description":"empty","descriptionFirstLine":"empty","parents":["p1"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"false","conflict":"false","empty":"true"}"#;
        let entries = parse_log_output(output).unwrap();
        assert!(entries[0].empty);
        assert!(!entries[0].conflict);
    }

    #[test]
    fn test_parse_log_conflicted_commit() {
        let output = r#"{"commitId":"abc","changeId":"xyz","authorName":"A","authorEmail":"a@b","description":"conflict","descriptionFirstLine":"conflict","parents":["p1"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"false","conflict":"true","empty":"false"}"#;
        let entries = parse_log_output(output).unwrap();
        assert!(entries[0].conflict);
    }

    #[test]
    fn test_parse_log_working_copy() {
        let output = r#"{"commitId":"abc","changeId":"xyz","authorName":"A","authorEmail":"a@b","description":"wip","descriptionFirstLine":"wip","parents":["p1"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"true","conflict":"false","empty":"false"}"#;
        let entries = parse_log_output(output).unwrap();
        assert!(entries[0].is_working_copy);
        assert!(entries[0].local_bookmarks.is_empty());
    }

    #[test]
    fn test_parse_log_merge_commit() {
        let output = r#"{"commitId":"abc","changeId":"xyz","authorName":"A","authorEmail":"a@b","description":"merge","descriptionFirstLine":"merge","parents":["p1","p2"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"false","conflict":"false","empty":"false"}"#;
        let entries = parse_log_output(output).unwrap();
        assert_eq!(entries[0].parents.len(), 2);
    }

    #[test]
    fn test_parse_log_empty_output() {
        let entries = parse_log_output("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_log_multiple_entries() {
        let output = concat!(
            r#"{"commitId":"a","changeId":"1","authorName":"A","authorEmail":"a@b","description":"first","descriptionFirstLine":"first","parents":["root"],"localBookmarks":["feat-a"],"remoteBookmarks":[],"isWorkingCopy":"false","conflict":"false","empty":"false"}"#,
            "\n",
            r#"{"commitId":"b","changeId":"2","authorName":"B","authorEmail":"b@c","description":"second","descriptionFirstLine":"second","parents":["a"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"true","conflict":"false","empty":"false"}"#,
            "\n",
        );
        let entries = parse_log_output(output).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].local_bookmarks, vec!["feat-a"]);
        assert!(entries[1].is_working_copy);
    }
}
