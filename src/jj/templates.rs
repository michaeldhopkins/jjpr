/// jj template strings for structured JSON output, and parsing logic.
use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::types::{Bookmark, GitRemote, LogEntry};

/// Template for `jj bookmark list` that produces line-delimited JSON.
///
/// jj applies it once per *ref*, not once per bookmark: a local bookmark gets a
/// line, and so does each of its tracked remote bookmarks whose target differs from
/// the local one (a synced remote bookmark gets none), and in a colocated repo a
/// `@git` ref that differs. So each line says which ref it is: `remote` is null for
/// a local bookmark and the remote's name otherwise. `remoteRefs` lists the remote
/// bookmarks on the target as raw `[name, remote]` pairs; a `name@remote` string
/// would carry jj's revset quoting (`"feat@v2"@origin`). Verified against jj 0.33
/// through 0.46 (`tests/jj_compat.rs`).
///
/// jj's escape_json() includes surrounding quotes, so values use it directly.
pub const BOOKMARK_TEMPLATE: &str = concat!(
    r#"'{"name":' ++ name.escape_json()"#,
    r#" ++ ',"remote":' ++ if(remote, remote.escape_json(), 'null')"#,
    r#" ++ ',"commitId":' ++ normal_target.commit_id().short().escape_json()"#,
    r#" ++ ',"changeId":' ++ normal_target.change_id().short().escape_json()"#,
    r#" ++ ',"remoteRefs":[' ++ normal_target.remote_bookmarks().map(|b| '[' ++ b.name().escape_json() ++ ',' ++ b.remote().escape_json() ++ ']').join(',') ++ ']'"#,
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

/// One line of [`BOOKMARK_TEMPLATE`] output.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBookmark {
    name: String,
    /// Null for a local bookmark, the remote's name for a remote ref. Required: a
    /// line without it is not this template's, and is treated as unreadable.
    remote: Option<String>,
    commit_id: String,
    change_id: String,
    remote_refs: Vec<(String, String)>,
}

/// The name, and the remote if the line is a remote ref's, from a line that is not
/// JSON. jj prints `<Error: No Commit available>` in place of the target's fields
/// for a conflicted ref, but `name` and `remote` come first and are whole JSON
/// values, so each is decoded as one: escaped quotes and backslashes survive.
fn name_of_malformed_line(line: &str) -> Option<(String, Option<String>)> {
    let after_key = line.split(r#""name":"#).nth(1)?;
    let mut values = serde_json::Deserializer::from_str(after_key).into_iter::<String>();
    let name = values.next()?.ok()?;
    let rest = &after_key[values.byte_offset()..];
    let remote = rest
        .strip_prefix(r#","remote":"#)
        .and_then(|r| {
            serde_json::Deserializer::from_str(r)
                .into_iter::<Option<String>>()
                .next()
        })
        .and_then(Result::ok)
        .flatten();
    Some((name, remote))
}

/// A remote that is a real forge remote, not colocation's `git`.
fn is_real_remote(remote: &str) -> bool {
    !remote.is_empty() && remote != "git"
}

/// Parse `jj bookmark list --template BOOKMARK_TEMPLATE` output into the local
/// bookmarks it lists, in order.
///
/// A remote ref's line is never a bookmark of its own; it only tells us where that
/// remote bookmark points. A bookmark `has_remote` when one of its own remote
/// bookmarks (not `@git`) exists: listed on a line of its own, which jj prints only
/// when it points elsewhere or is conflicted, or on the local target. It
/// `is_synced` when it has one and every one of them is on the local target.
///
/// Returns `(bookmarks, warnings)` where `warnings` names the bookmarks skipped
/// because jj could not render their local target: a conflicted bookmark, or one
/// pointing at a missing commit (typically after a squash merge on the forge).
pub fn parse_bookmark_output(output: &str) -> Result<(Vec<Bookmark>, Vec<String>)> {
    let mut locals: Vec<RawBookmark> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut seen_unknown_malformed = false;
    // Where each bookmark's remote refs point: `None` for one jj could not render.
    let mut remote_targets: HashMap<String, Vec<Option<String>>> = HashMap::new();

    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        let (name, remote) = match serde_json::from_str::<RawBookmark>(line) {
            // A line can be valid JSON and still carry no usable identity — an
            // empty `changeId` or `commitId`. Without this check the bookmark
            // reaches the change graph with an empty change id, and from there
            // `rebase_root` hands "" to revset construction (`change_id()::name`,
            // a syntax error) and to `jj rebase -s ''`. Not producible by jj
            // today, whose templates always emit both; a trust-boundary guard,
            // found by the `graph_invariants` fuzz target.
            Ok(raw) if !raw.change_id.is_empty() && !raw.commit_id.is_empty() => {
                match &raw.remote {
                    None => locals.push(raw),
                    Some(remote) if is_real_remote(remote) => remote_targets
                        .entry(raw.name.clone())
                        .or_default()
                        .push(Some(raw.commit_id.clone())),
                    Some(_) => {}
                }
                continue;
            }
            Ok(raw) => (raw.name, raw.remote),
            Err(_) => match name_of_malformed_line(line) {
                Some(found) => found,
                None => {
                    seen_unknown_malformed = true;
                    continue;
                }
            },
        };
        match remote {
            Some(remote) if is_real_remote(&remote) => {
                remote_targets.entry(name).or_default().push(None);
            }
            Some(_) => {}
            None if name.is_empty() => seen_unknown_malformed = true,
            None => {
                if !warnings.contains(&name) {
                    warnings.push(name);
                }
            }
        }
    }

    if seen_unknown_malformed {
        eprintln!("  Warning: skipping unparseable bookmark entry");
    }

    let bookmarks = locals
        .into_iter()
        .map(|raw| {
            let elsewhere = remote_targets.get(&raw.name).map_or(&[][..], Vec::as_slice);
            let here = raw
                .remote_refs
                .iter()
                .any(|(name, remote)| *name == raw.name && is_real_remote(remote));
            let has_remote = here || !elsewhere.is_empty();
            let is_synced = has_remote
                && elsewhere
                    .iter()
                    .all(|target| target.as_deref() == Some(raw.commit_id.as_str()));
            Bookmark {
                name: raw.name,
                commit_id: raw.commit_id,
                change_id: raw.change_id,
                has_remote,
                is_synced,
            }
        })
        .collect();

    Ok((bookmarks, warnings))
}

/// Template for `jj log -r trunk()`: the names of the remote bookmarks on trunk,
/// comma-separated.
pub const TRUNK_BOOKMARKS_TEMPLATE: &str = r#"remote_bookmarks.map(|b| b.name()).join(",")"#;

/// The default branch's name from [`TRUNK_BOOKMARKS_TEMPLATE`] output, or `None`
/// when trunk carries no remote bookmark.
///
/// jj lists the names alphabetically, so a branch that shares trunk's commit (a
/// fast-forwarded PR branch not yet deleted, `landed`) comes before `main`. The
/// names jj's built-in `trunk()` looks for win, in its order; otherwise the first.
pub fn parse_default_branch(output: &str) -> Option<String> {
    let names: Vec<&str> = output
        .trim()
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect();
    ["main", "master", "trunk"]
        .into_iter()
        .find(|preferred| names.contains(preferred))
        .or_else(|| names.first().copied())
        .map(str::to_string)
}

/// Parse `jj git remote list` output: one `<name> <url>` per line.
pub fn parse_remote_list(output: &str) -> Vec<GitRemote> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let name = parts.next()?.trim().to_string();
            let url = parts.next()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(GitRemote { name, url })
        })
        .collect()
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

    const ERR: &str = "<Error: No Commit available>";

    /// A local bookmark's line. `refs` is the remote bookmarks on its target, as
    /// `(name, remote)`.
    fn local(name: &str, commit: &str, refs: &[(&str, &str)]) -> String {
        let refs: Vec<String> = refs
            .iter()
            .map(|(n, r)| format!(r#"["{n}","{r}"]"#))
            .collect();
        format!(
            r#"{{"name":"{name}","remote":null,"commitId":"{commit}","changeId":"ch-{commit}","remoteRefs":[{}]}}"#,
            refs.join(",")
        )
    }

    /// A remote ref's line: `name@remote` pointing at `commit`, whose target carries
    /// `refs`.
    fn remote(name: &str, remote: &str, commit: &str, refs: &[(&str, &str)]) -> String {
        local(name, commit, refs).replace(r#""remote":null"#, &format!(r#""remote":"{remote}""#))
    }

    /// A line jj could not render: its target is conflicted or missing.
    fn unrenderable(name: &str, remote: Option<&str>) -> String {
        let remote = remote.map_or("null".to_string(), |r| format!(r#""{r}""#));
        format!(
            r#"{{"name":"{name}","remote":{remote},"commitId":{ERR},"changeId":{ERR},"remoteRefs":[{ERR}]}}"#
        )
    }

    fn parse(lines: &[String]) -> (Vec<Bookmark>, Vec<String>) {
        parse_bookmark_output(&lines.join("\n")).unwrap()
    }

    fn status(b: &Bookmark) -> (&str, bool, bool) {
        (b.name.as_str(), b.has_remote, b.is_synced)
    }

    #[test]
    fn test_parse_bookmark_no_remote() {
        let (bookmarks, warnings) = parse(&[local("feature", "abc123", &[])]);
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "feature");
        assert_eq!(bookmarks[0].commit_id, "abc123");
        assert_eq!(bookmarks[0].change_id, "ch-abc123");
        assert!(!bookmarks[0].has_remote);
        assert!(!bookmarks[0].is_synced);
        assert!(warnings.is_empty());
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
            r#"{"name":"feat","remote":null,"commitId":"c0","changeId":"","remoteRefs":[]}"#,
            r#"{"name":"feat","remote":null,"commitId":"","changeId":"ch0","remoteRefs":[]}"#,
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

    // A remote ref with no usable identity says only that the remote points
    // somewhere jjpr cannot see: the local bookmark is kept, unsynced, unwarned.
    #[test]
    fn a_remote_ref_with_no_identity_leaves_the_bookmark_unsynced() {
        let output = [
            r#"{"name":"feat","remote":"origin","commitId":"c0","changeId":"","remoteRefs":[]}"#
                .to_string(),
            local("feat", "c1", &[]),
        ];
        let (bookmarks, warnings) = parse(&output);
        assert_eq!(bookmarks.len(), 1, "the local entry is kept");
        assert_eq!(bookmarks[0].change_id, "ch-c1");
        assert_eq!(status(&bookmarks[0]), ("feat", true, false));
        assert!(warnings.is_empty(), "no warning: {warnings:?}");
    }

    #[test]
    fn test_parse_bookmark_with_synced_remote() {
        let (bookmarks, _) = parse(&[local("feature", "abc", &[("feature", "origin")])]);
        assert_eq!(status(&bookmarks[0]), ("feature", true, true));
    }

    #[test]
    fn test_parse_bookmark_with_git_remote_only() {
        let (bookmarks, _) = parse(&[local("feature", "abc", &[("feature", "git")])]);
        assert_eq!(
            status(&bookmarks[0]),
            ("feature", false, false),
            "@git refs are colocation, not a remote"
        );
    }

    // Before the template said which ref a line is, a bookmark sharing a commit
    // with ANOTHER bookmark's remote read as pushed: `local-only` at B, where
    // `feature@origin` also points, showed "push needs updating" when it had never
    // been pushed.
    #[test]
    fn another_bookmarks_remote_on_the_target_is_not_this_ones() {
        let (bookmarks, _) = parse(&[local(
            "local-only",
            "b",
            &[("feature", "origin"), ("local-only", "git")],
        )]);
        assert_eq!(status(&bookmarks[0]), ("local-only", false, false));
    }

    // jj prints a tracked remote bookmark on a line of its own when it points
    // elsewhere. That line used to become a second, synced `feature` at the old
    // commit whenever that commit carried any local bookmark, and the local
    // `feature`, whose target has no `feature@origin`, read as never pushed.
    // Captured from jj 0.33 through 0.46 in tests/fixtures/jj/.
    #[test]
    fn a_remote_that_points_elsewhere_is_not_a_bookmark_and_unsyncs_the_local_one() {
        let output = [
            local("feature", "c", &[("feature", "git")]),
            remote(
                "feature",
                "origin",
                "b",
                &[("feature", "origin"), ("local-only", "git")],
            ),
            local(
                "local-only",
                "b",
                &[("feature", "origin"), ("local-only", "git")],
            ),
        ];
        let (bookmarks, warnings) = parse(&output);
        let got: Vec<_> = bookmarks.iter().map(status).collect();
        assert_eq!(
            got,
            vec![("feature", true, false), ("local-only", false, false)]
        );
        assert_eq!(
            bookmarks[0].commit_id, "c",
            "the local target, not the remote's"
        );
        assert!(warnings.is_empty());
    }

    // Colocation's `@git` ref is not a remote. jj prints its own line for it when
    // it differs from the local target (a conflicted bookmark's does), rendered or
    // not; neither may make a synced bookmark unsynced, or give a never-pushed
    // bookmark a remote.
    #[test]
    fn a_git_ref_line_is_not_a_remote() {
        for git_line in [
            remote("feature", "git", "elsewhere", &[("feature", "git")]),
            unrenderable("feature", Some("git")),
        ] {
            let output = [
                local("feature", "c", &[("feature", "origin")]),
                git_line.clone(),
            ];
            let (bookmarks, warnings) = parse(&output);
            assert_eq!(status(&bookmarks[0]), ("feature", true, true), "{git_line}");
            assert!(warnings.is_empty(), "{git_line}: {warnings:?}");

            let output = [local("feature", "c", &[]), git_line.clone()];
            let (bookmarks, _) = parse(&output);
            assert_eq!(
                status(&bookmarks[0]),
                ("feature", false, false),
                "{git_line}"
            );
        }
    }

    // A local line with no name cannot be named in a warning. It is dropped with
    // the generic "unparseable entry" notice rather than warned about as "".
    #[test]
    fn a_nameless_unreadable_local_line_is_not_warned_by_name() {
        for line in [
            r#"{"name":"","remote":null,"commitId":"","changeId":"","remoteRefs":[]}"#.to_string(),
            unrenderable("", None),
        ] {
            let (bookmarks, warnings) = parse_bookmark_output(&line).unwrap();
            assert!(bookmarks.is_empty(), "{line}");
            assert!(warnings.is_empty(), "{line}: {warnings:?}");
        }
    }

    #[test]
    fn a_remote_line_on_the_local_target_counts_as_synced() {
        // `--all-remotes` style: the remote ref listed although it matches.
        let output = [
            local("feature", "c", &[]),
            remote("feature", "origin", "c", &[("feature", "origin")]),
        ];
        let (bookmarks, _) = parse(&output);
        assert_eq!(status(&bookmarks[0]), ("feature", true, true));
    }

    // A conflicted bookmark in a colocated repo has an unrenderable local line and
    // a renderable `@git` line. The `@git` line used to be taken for the bookmark,
    // silently resolving the conflict to one side with no warning.
    #[test]
    fn a_conflicted_bookmark_is_skipped_even_with_a_git_ref_line() {
        let output = [
            unrenderable("conflicted", None),
            remote(
                "conflicted",
                "git",
                "b",
                &[("conflicted", "git"), ("local-only", "git")],
            ),
            local("feat/good", "abc", &[("feat/good", "origin")]),
        ];
        let (bookmarks, warnings) = parse(&output);
        let got: Vec<_> = bookmarks.iter().map(status).collect();
        assert_eq!(got, vec![("feat/good", true, true)]);
        assert_eq!(warnings, vec!["conflicted".to_string()]);
    }

    // A bookmark deleted locally but still tracked on a remote has no local target;
    // its remote line must not bring it back.
    #[test]
    fn a_locally_deleted_tracked_bookmark_is_not_reported() {
        let output = [
            unrenderable("feat@v2", None),
            remote(
                "feat@v2",
                "origin",
                "a",
                &[("feat@v2", "origin"), ("synced", "origin")],
            ),
            local(
                "synced",
                "a",
                &[("feat@v2", "origin"), ("synced", "origin")],
            ),
        ];
        let (bookmarks, warnings) = parse(&output);
        let got: Vec<_> = bookmarks.iter().map(status).collect();
        assert_eq!(got, vec![("synced", true, true)]);
        assert_eq!(warnings, vec!["feat@v2".to_string()]);
    }

    /// Regression test for a false-positive "skipping" warning observed on a
    /// real PR: the bookmark had a healthy local target plus a stale `@origin`
    /// target whose commit had been abandoned. The unrenderable remote line
    /// must neither warn nor drop the bookmark; it does mean the remote is not
    /// where the local bookmark is.
    #[test]
    fn an_unrenderable_remote_line_keeps_the_bookmark_unsynced_and_unwarned() {
        for output in [
            [
                local("feature", "good_local", &[("feature", "git")]),
                unrenderable("feature", Some("origin")),
            ],
            [
                unrenderable("feature", Some("origin")),
                local("feature", "good_local", &[("feature", "git")]),
            ],
        ] {
            let (bookmarks, warnings) = parse(&output);
            assert_eq!(bookmarks.len(), 1, "{output:?}");
            assert_eq!(bookmarks[0].commit_id, "good_local");
            assert_eq!(status(&bookmarks[0]), ("feature", true, false));
            assert!(warnings.is_empty(), "{warnings:?}");
        }
    }

    #[test]
    fn test_parse_bookmark_multiple_in_order() {
        let output = [
            local("auth", "aaa", &[("auth", "origin")]),
            local("profile", "bbb", &[]),
        ];
        let (bookmarks, _) = parse(&output);
        let got: Vec<_> = bookmarks.iter().map(status).collect();
        assert_eq!(got, vec![("auth", true, true), ("profile", false, false)]);
    }

    /// A bookmark whose local line appears unrenderable more than once produces
    /// one warning.
    #[test]
    fn test_parse_bookmark_dedupes_warnings() {
        let output = [
            unrenderable("feat/dead", None),
            unrenderable("feat/dead", None),
        ];
        let (bookmarks, warnings) = parse(&output);
        assert!(bookmarks.is_empty());
        assert_eq!(warnings, vec!["feat/dead".to_string()]);
    }

    #[test]
    fn the_name_of_an_unrenderable_line_is_decoded_as_json() {
        let line = format!(
            r#"{{"name":"a\"b\\c","remote":"up\"stream","commitId":{ERR},"changeId":{ERR},"remoteRefs":[{ERR}]}}"#
        );
        assert_eq!(
            name_of_malformed_line(&line),
            Some((r#"a"b\c"#.to_string(), Some(r#"up"stream"#.to_string())))
        );
        assert_eq!(
            name_of_malformed_line(&unrenderable("x", None)),
            Some(("x".to_string(), None))
        );
        assert_eq!(name_of_malformed_line("garbage"), None);
    }

    // Output of the template before it said which ref a line is has no `remote`,
    // so a line of it cannot be placed: it is skipped by name, never guessed at.
    #[test]
    fn a_line_without_remote_is_unreadable() {
        let old = r#"{"name":"feature","commitId":"abc","changeId":"xyz","localBookmarks":["feature"],"remoteBookmarks":["feature@origin"]}"#;
        let (bookmarks, warnings) = parse_bookmark_output(old).unwrap();
        assert!(bookmarks.is_empty());
        assert_eq!(warnings, vec!["feature".to_string()]);
    }

    #[test]
    fn test_parse_bookmark_empty_output() {
        let (bookmarks, warnings) = parse_bookmark_output("").unwrap();
        assert!(bookmarks.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn default_branch_is_the_first_remote_bookmark_on_trunk() {
        assert_eq!(
            parse_default_branch("main,main\n"),
            Some("main".to_string())
        );
        assert_eq!(
            parse_default_branch(" develop "),
            Some("develop".to_string())
        );
        assert_eq!(parse_default_branch(""), None);
        assert_eq!(parse_default_branch(",main"), Some("main".to_string()));
        // Captured on jj 0.33 through 0.46: a second branch on trunk's commit sorts
        // first, and used to be taken for the default branch.
        assert_eq!(
            parse_default_branch("landed,landed,main,main"),
            Some("main".to_string())
        );
        assert_eq!(
            parse_default_branch("a,master,trunk"),
            Some("master".to_string())
        );
    }

    #[test]
    fn remote_list_is_name_then_url() {
        let remotes = parse_remote_list("origin git@github.com:o/r.git\nup https://x/y z\n\n");
        let got: Vec<_> = remotes
            .iter()
            .map(|r| (r.name.as_str(), r.url.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("origin", "git@github.com:o/r.git"),
                ("up", "https://x/y z")
            ]
        );
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
