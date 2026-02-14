/// jj template strings for structured JSON output, and parsing logic.
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
    r#" ++ '}' ++ "\n""#,
);

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
pub fn parse_bookmark_output(output: &str) -> Result<Vec<Bookmark>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let raw: RawBookmark =
                serde_json::from_str(line)
                    .with_context(|| format!("failed to parse bookmark JSON: {line}"))?;

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

            Ok((raw.local_bookmarks.is_empty(), Bookmark {
                name: raw.name,
                commit_id: raw.commit_id,
                change_id: raw.change_id,
                has_remote,
                is_synced,
            }))
        })
        .filter_map(|result| match result {
            Ok((true, _)) => None, // Skip remote-only entries
            Ok((false, bookmark)) => Some(Ok(bookmark)),
            Err(e) => Some(Err(e)),
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
}

/// Parse `jj log` output into `LogEntry` values.
pub fn parse_log_output(output: &str) -> Result<Vec<LogEntry>> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let raw: RawLogEntry =
                serde_json::from_str(line)
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
        let bookmarks = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "feature");
        assert_eq!(bookmarks[0].commit_id, "abc123");
        assert!(!bookmarks[0].has_remote);
        assert!(!bookmarks[0].is_synced);
    }

    #[test]
    fn test_parse_bookmark_with_synced_remote() {
        let output = r#"{"name":"feature","commitId":"abc123","changeId":"xyz789","localBookmarks":["feature"],"remoteBookmarks":["feature@origin"]}"#;
        let bookmarks = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1);
        assert!(bookmarks[0].has_remote);
        assert!(bookmarks[0].is_synced);
    }

    #[test]
    fn test_parse_bookmark_with_git_remote_only() {
        let output = r#"{"name":"feature","commitId":"abc123","changeId":"xyz789","localBookmarks":["feature"],"remoteBookmarks":["feature@git"]}"#;
        let bookmarks = parse_bookmark_output(output).unwrap();
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
        let bookmarks = parse_bookmark_output(output).unwrap();
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
        let bookmarks = parse_bookmark_output(output).unwrap();
        assert_eq!(bookmarks.len(), 1, "should filter out remote-only entry");
        assert_eq!(bookmarks[0].commit_id, "new111", "should keep local target");
        assert!(!bookmarks[0].is_synced, "divergent bookmark is not synced");
        assert!(!bookmarks[0].has_remote, "local entry lacks @origin");
    }

    #[test]
    fn test_parse_bookmark_empty_output() {
        let bookmarks = parse_bookmark_output("").unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn test_parse_log_entry() {
        let output = r#"{"commitId":"abc123","changeId":"xyz789","authorName":"Alice","authorEmail":"alice@example.com","description":"Add feature\n\nDetailed description","descriptionFirstLine":"Add feature","parents":["def456"],"localBookmarks":["feature"],"remoteBookmarks":[],"isWorkingCopy":"false"}"#;
        let entries = parse_log_output(output).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].commit_id, "abc123");
        assert_eq!(entries[0].description_first_line, "Add feature");
        assert_eq!(entries[0].parents, vec!["def456"]);
        assert!(!entries[0].is_working_copy);
    }

    #[test]
    fn test_parse_log_working_copy() {
        let output = r#"{"commitId":"abc","changeId":"xyz","authorName":"A","authorEmail":"a@b","description":"wip","descriptionFirstLine":"wip","parents":["p1"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"true"}"#;
        let entries = parse_log_output(output).unwrap();
        assert!(entries[0].is_working_copy);
        assert!(entries[0].local_bookmarks.is_empty());
    }

    #[test]
    fn test_parse_log_merge_commit() {
        let output = r#"{"commitId":"abc","changeId":"xyz","authorName":"A","authorEmail":"a@b","description":"merge","descriptionFirstLine":"merge","parents":["p1","p2"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"false"}"#;
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
            r#"{"commitId":"a","changeId":"1","authorName":"A","authorEmail":"a@b","description":"first","descriptionFirstLine":"first","parents":["root"],"localBookmarks":["feat-a"],"remoteBookmarks":[],"isWorkingCopy":"false"}"#,
            "\n",
            r#"{"commitId":"b","changeId":"2","authorName":"B","authorEmail":"b@c","description":"second","descriptionFirstLine":"second","parents":["a"],"localBookmarks":[],"remoteBookmarks":[],"isWorkingCopy":"true"}"#,
            "\n",
        );
        let entries = parse_log_output(output).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].local_bookmarks, vec!["feat-a"]);
        assert!(entries[1].is_working_copy);
    }
}
