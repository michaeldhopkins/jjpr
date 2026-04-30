/// Stack navigation generation, parsing, and in-place editing.
///
/// The `StackNav` trait abstracts where stack navigation lives (comment vs PR
/// description). `CommentNav` is the default — it stores stack info as a PR
/// comment. Alternative implementations can store it elsewhere.
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

use super::Forge;
use super::types::{IssueComment, PullRequest};

const SENTINEL: &str = "<!-- jjpr:stack-info -->";
const FOOTER: &str = "*Created with [jjpr](https://github.com/michaeldhopkins/jjpr)*";
// Also detect jj-stack comments for migration
const LEGACY_FOOTER: &str = "*Created with [jj-stack]";

/// Machine-readable state embedded in the comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackCommentData {
    pub version: u32,
    pub stack: Vec<StackCommentItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackCommentItem {
    pub bookmark_name: String,
    pub pr_url: String,
    pub pr_number: u64,
    #[serde(default)]
    pub is_merged: bool,
    /// ISO-8601 timestamp from the forge marking when the PR became
    /// non-open (`merged_at` for merged PRs, `closed_at` for closed-without-
    /// merge). Used to sort fossils. Persists in JJPR_DATA so subsequent
    /// developers running submit inherit the timestamp without re-querying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
}

/// Entry for rendering the stack comment.
pub struct StackEntry {
    pub bookmark_name: String,
    pub pr_url: Option<String>,
    pub pr_number: Option<u64>,
    pub is_current: bool,
    pub is_merged: bool,
    pub closed_at: Option<String>,
}

/// Maximum number of fossil entries (closed/merged PRs no longer in the
/// live local stack) to display in the `<details>` block. Older fossils
/// beyond this cap are dropped from the rendered comment, but their data
/// continues to live in `JJPR_DATA` so we can display them again if
/// older fossils later become relevant (unlikely, but cheap to preserve).
pub const FOSSIL_DISPLAY_CAP: usize = 7;

/// Generate the body for a stack navigation comment.
///
/// `live` is rendered as the primary numbered list (open PRs in stack
/// order). `fossils` is rendered inside a collapsible `<details>` block
/// at the bottom — closed and merged PRs that are no longer part of
/// the live local stack, capped to the most recent
/// `FOSSIL_DISPLAY_CAP` entries by closed_at timestamp.
pub fn generate_comment_body(live: &[StackEntry], fossils: &[StackEntry]) -> String {
    let display_fossils: Vec<&StackEntry> = fossils.iter().take(FOSSIL_DISPLAY_CAP).collect();

    // Persist every fossil we know about (including ones we're not
    // displaying) so future runs can still see them. Live entries sort
    // first, fossils after.
    let mut stack_items = Vec::with_capacity(live.len() + fossils.len());
    for entry in live.iter().chain(fossils.iter()) {
        if let (Some(url), Some(number)) = (&entry.pr_url, entry.pr_number) {
            stack_items.push(StackCommentItem {
                bookmark_name: entry.bookmark_name.clone(),
                pr_url: url.clone(),
                pr_number: number,
                is_merged: entry.is_merged,
                closed_at: entry.closed_at.clone(),
            });
        }
    }

    let data = StackCommentData {
        version: 1,
        stack: stack_items,
    };

    let json = serde_json::to_string(&data).expect("StackCommentData serialization cannot fail");
    let encoded = BASE64.encode(json.as_bytes());

    let mut body = String::new();
    body.push_str(SENTINEL);
    body.push('\n');
    body.push_str(&format!("<!--- JJPR_DATA: {encoded} --->"));
    body.push('\n');
    body.push_str("This PR is part of a stack:\n\n");

    for entry in live {
        if entry.is_current {
            body.push_str(&format!("1. **`{}` <-- this PR**\n", entry.bookmark_name));
        } else if let Some(url) = &entry.pr_url {
            body.push_str(&format!("1. [`{}`]({url})\n", entry.bookmark_name));
        } else {
            body.push_str(&format!("1. `{}`\n", entry.bookmark_name));
        }
    }

    if !display_fossils.is_empty() {
        let total = fossils.len();
        let shown = display_fossils.len();
        let pr_label = if shown == 1 { "PR" } else { "PRs" };
        let summary_line = if total > shown {
            let hidden = total - shown;
            let entry_label = if hidden == 1 { "entry" } else { "entries" };
            format!(
                "{shown} earlier closed/merged {pr_label} (+{hidden} older {entry_label} hidden)"
            )
        } else {
            format!("{shown} earlier closed/merged {pr_label}")
        };
        body.push_str(&format!("\n<details><summary>{summary_line}</summary>\n\n"));
        for entry in display_fossils {
            if let Some(url) = &entry.pr_url {
                body.push_str(&format!("1. ~~[`{}`]({url})~~\n", entry.bookmark_name));
            } else {
                body.push_str(&format!("1. ~~`{}`~~\n", entry.bookmark_name));
            }
        }
        body.push_str("\n</details>\n");
    }

    body.push_str(&format!("\n---\n{FOOTER}\n"));
    body
}

/// Parse the machine-readable data from an existing stack comment.
pub fn parse_comment_data(body: &str) -> Option<StackCommentData> {
    let suffix = " --->";

    for line in body.lines() {
        let line = line.trim();
        let encoded = line
            .strip_prefix("<!--- JJPR_DATA: ")
            .or_else(|| line.strip_prefix("<!--- STACKER_DATA: "))
            .and_then(|rest| rest.strip_suffix(suffix));

        if let Some(encoded) = encoded {
            let bytes = BASE64.decode(encoded).ok()?;
            let data: StackCommentData = serde_json::from_slice(&bytes).ok()?;
            return Some(data);
        }
    }
    None
}

/// Find an existing jjpr (or legacy jj-stack) comment in a list of comments.
pub fn find_stack_comment(comments: &[IssueComment]) -> Option<&IssueComment> {
    comments.iter().find(|c| {
        let body = c.body.as_deref().unwrap_or("");
        body.contains(SENTINEL) || body.contains(LEGACY_FOOTER)
    })
}

/// Callback signature for `StackNav::update`. Given the previous comment's
/// data (if any), produce `(live, fossils)` for rendering.
pub type BuildEntriesFn<'a> =
    dyn Fn(Option<&StackCommentData>) -> (Vec<StackEntry>, Vec<StackEntry>) + 'a;

/// Adapter for reading and writing stack navigation on PRs.
///
/// Each method that touches the forge should make at most one API call
/// for the navigation data. Callers use `has_existing` for the skip check
/// and `update` for the full read-merge-write cycle.
pub trait StackNav: Send + Sync {
    /// Check if this PR already has stack nav content.
    fn has_existing(
        &self,
        forge: &dyn Forge,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
    ) -> Result<bool>;

    /// Read existing data, merge with new entries, and write the result.
    /// Returns true if content was written or updated.
    ///
    /// `build_entries` receives the existing stack data (if any) and
    /// returns `(live, fossils)`: open PRs and closed/merged PRs that
    /// belong in the collapsible history block, respectively.
    fn update(
        &self,
        forge: &dyn Forge,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
        build_entries: &BuildEntriesFn<'_>,
    ) -> Result<bool>;
}

/// Store stack navigation as a PR comment (default behavior).
pub struct CommentNav;

impl StackNav for CommentNav {
    fn has_existing(
        &self,
        forge: &dyn Forge,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
    ) -> Result<bool> {
        let comments = forge.list_comments(owner, repo, pr.number)?;
        Ok(find_stack_comment(&comments).is_some())
    }

    fn update(
        &self,
        forge: &dyn Forge,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
        build_entries: &BuildEntriesFn<'_>,
    ) -> Result<bool> {
        // Single list_comments call — used for reading existing data AND writing
        let comments = forge.list_comments(owner, repo, pr.number)?;
        let existing = find_stack_comment(&comments);

        let previous_data = existing
            .and_then(|c| c.body.as_deref())
            .and_then(parse_comment_data);

        let (live, fossils) = build_entries(previous_data.as_ref());
        if live.is_empty() && fossils.is_empty() {
            return Ok(false);
        }
        let body = generate_comment_body(&live, &fossils);

        if let Some(existing_comment) = existing {
            if existing_comment.body.as_deref() != Some(&body) {
                forge.update_comment(owner, repo, existing_comment.id, &body)?;
                return Ok(true);
            }
            Ok(false)
        } else {
            forge.create_comment(owner, repo, pr.number, &body)?;
            Ok(true)
        }
    }
}

const NAV_START: &str = "<!-- jjpr:stack-nav -->";
const NAV_END: &str = "<!-- /jjpr:stack-nav -->";

/// Store stack navigation in the PR description/body.
pub struct DescriptionNav;

impl DescriptionNav {
    fn extract_section(body: &str) -> Option<&str> {
        let start_idx = body.find(NAV_START)?;
        let end_tag_start = body[start_idx..].find(NAV_END)?;
        let end_idx = start_idx + end_tag_start + NAV_END.len();
        Some(&body[start_idx..end_idx])
    }

    fn splice_section(body: &str, new_section: &str) -> String {
        if let Some(start_idx) = body.find(NAV_START)
            && let Some(end_tag_start) = body[start_idx..].find(NAV_END)
        {
            let end_idx = start_idx + end_tag_start + NAV_END.len();
            let before = &body[..start_idx];
            let after = &body[end_idx..];
            format!("{before}{new_section}{after}")
        } else {
            // Append at the end, separated by blank lines
            let trimmed = body.trim_end();
            if trimmed.is_empty() {
                new_section.to_string()
            } else {
                format!("{trimmed}\n\n{new_section}\n")
            }
        }
    }

    fn wrap_section(content: &str) -> String {
        format!("{NAV_START}\n{content}{NAV_END}")
    }
}

impl StackNav for DescriptionNav {
    fn has_existing(
        &self,
        _forge: &dyn Forge,
        _owner: &str,
        _repo: &str,
        pr: &PullRequest,
    ) -> Result<bool> {
        Ok(pr.body.as_deref().is_some_and(|b| b.contains(NAV_START)))
    }

    fn update(
        &self,
        forge: &dyn Forge,
        owner: &str,
        repo: &str,
        pr: &PullRequest,
        build_entries: &BuildEntriesFn<'_>,
    ) -> Result<bool> {
        let current_body = pr.body.as_deref().unwrap_or("");

        let previous_data = Self::extract_section(current_body)
            .and_then(parse_comment_data);

        let (live, fossils) = build_entries(previous_data.as_ref());
        if live.is_empty() && fossils.is_empty() {
            return Ok(false);
        }

        let nav_content = generate_comment_body(&live, &fossils);
        let new_section = Self::wrap_section(&nav_content);

        let new_body = Self::splice_section(current_body, &new_section);

        if new_body.trim() == current_body.trim() {
            return Ok(false);
        }

        forge.update_pr_body(owner, repo, pr.number, &new_body)?;
        Ok(true)
    }
}

/// Create a StackNav adapter for the given mode.
pub fn create_stack_nav(mode: crate::config::StackNavMode) -> Box<dyn StackNav> {
    match mode {
        crate::config::StackNavMode::Comment => Box::new(CommentNav),
        crate::config::StackNavMode::Description => Box::new(DescriptionNav),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_entry(name: &str, number: u64, is_current: bool) -> StackEntry {
        StackEntry {
            bookmark_name: name.to_string(),
            pr_url: Some(format!("https://github.com/o/r/pull/{number}")),
            pr_number: Some(number),
            is_current,
            is_merged: false,
            closed_at: None,
        }
    }

    fn fossil_entry(name: &str, number: u64, closed_at: &str) -> StackEntry {
        StackEntry {
            bookmark_name: name.to_string(),
            pr_url: Some(format!("https://github.com/o/r/pull/{number}")),
            pr_number: Some(number),
            is_current: false,
            is_merged: true,
            closed_at: Some(closed_at.to_string()),
        }
    }

    fn sample_live() -> Vec<StackEntry> {
        vec![
            live_entry("auth", 1, false),
            live_entry("profile", 2, true),
            StackEntry {
                bookmark_name: "settings".to_string(),
                pr_url: None,
                pr_number: None,
                is_current: false,
                is_merged: false,
                closed_at: None,
            },
        ]
    }

    #[test]
    fn test_generate_comment_body_contains_sentinel() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(body.contains(SENTINEL));
    }

    #[test]
    fn test_generate_comment_body_contains_footer() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(body.contains(FOOTER));
    }

    #[test]
    fn test_generate_comment_body_marks_current_pr() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(body.contains("**`profile` <-- this PR**"));
    }

    #[test]
    fn test_generate_comment_body_links_other_prs() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(body.contains("1. [`auth`](https://github.com/o/r/pull/1)\n"));
    }

    #[test]
    fn test_generate_comment_body_shows_unlinked_bookmarks() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(body.contains("`settings`"));
    }

    #[test]
    fn test_generate_comment_body_excludes_default_branch() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(!body.contains("1. `main`"));
    }

    #[test]
    fn test_no_fossils_means_no_details_block() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(!body.contains("<details>"));
        assert!(!body.contains("earlier closed/merged"));
    }

    #[test]
    fn test_fossils_render_inside_details_block() {
        let live = vec![live_entry("top", 5, true)];
        let fossils = vec![fossil_entry("old1", 1, "2026-01-01T00:00:00Z")];
        let body = generate_comment_body(&live, &fossils);
        assert!(body.contains("<details><summary>1 earlier closed/merged PR</summary>"));
        assert!(body.contains("</details>"));
        // Fossil rendered as strikethrough link, no icon
        assert!(body.contains("1. ~~[`old1`](https://github.com/o/r/pull/1)~~\n"));
    }

    #[test]
    fn test_fossil_summary_pluralizes_correctly() {
        let live = vec![live_entry("top", 5, true)];
        let fossils = vec![
            fossil_entry("old1", 1, "2026-01-01T00:00:00Z"),
            fossil_entry("old2", 2, "2026-01-02T00:00:00Z"),
        ];
        let body = generate_comment_body(&live, &fossils);
        assert!(body.contains("2 earlier closed/merged PRs"));
    }

    #[test]
    fn test_fossil_cap_truncates_to_seven_with_hidden_count() {
        let live = vec![live_entry("top", 100, true)];
        let fossils: Vec<StackEntry> = (1..=10)
            .map(|i| fossil_entry(&format!("old{i}"), i, &format!("2026-01-{:02}T00:00:00Z", i)))
            .collect();
        let body = generate_comment_body(&live, &fossils);
        assert!(
            body.contains("7 earlier closed/merged PRs (+3 older entries hidden)"),
            "expected truncation indicator, got body:\n{body}"
        );
        // First 7 fossils render
        for i in 1..=7 {
            assert!(
                body.contains(&format!("~~[`old{i}`]")),
                "fossil old{i} should render"
            );
        }
        // 8-10 don't render in the visible list
        for i in 8..=10 {
            assert!(
                !body.contains(&format!("~~[`old{i}`]")),
                "fossil old{i} should not render past the cap"
            );
        }
    }

    #[test]
    fn test_fossil_cap_one_hidden_uses_singular_entry() {
        let live = vec![live_entry("top", 100, true)];
        let fossils: Vec<StackEntry> = (1..=8)
            .map(|i| fossil_entry(&format!("old{i}"), i, &format!("2026-01-{:02}T00:00:00Z", i)))
            .collect();
        let body = generate_comment_body(&live, &fossils);
        assert!(
            body.contains("(+1 older entry hidden)"),
            "expected singular 'entry', got body:\n{body}"
        );
    }

    #[test]
    fn test_fossils_persist_in_jjpr_data_even_when_truncated() {
        // Even when only 7 are visually rendered, all known fossils are
        // preserved in JJPR_DATA so future runs can re-derive history.
        let live = vec![live_entry("top", 100, true)];
        let fossils: Vec<StackEntry> = (1..=10)
            .map(|i| fossil_entry(&format!("old{i}"), i, &format!("2026-01-{:02}T00:00:00Z", i)))
            .collect();
        let body = generate_comment_body(&live, &fossils);
        let data = parse_comment_data(&body).expect("data should round-trip");
        // 1 live + 10 fossils = 11 stack items in the embedded data
        assert_eq!(data.stack.len(), 11);
        // closed_at preserved
        let old10 = data
            .stack
            .iter()
            .find(|item| item.bookmark_name == "old10")
            .expect("old10 in data");
        assert_eq!(old10.closed_at.as_deref(), Some("2026-01-10T00:00:00Z"));
        assert!(old10.is_merged);
    }

    #[test]
    fn test_roundtrip_comment_data() {
        let body = generate_comment_body(&sample_live(), &[]);
        let data = parse_comment_data(&body).expect("should parse embedded data");
        assert_eq!(data.version, 1);
        // Only entries with a PR number (auth + profile) end up in the
        // serialized stack; settings has no PR data.
        assert_eq!(data.stack.len(), 2);
        assert_eq!(data.stack[0].bookmark_name, "auth");
        assert_eq!(data.stack[0].pr_number, 1);
        assert!(!data.stack[0].is_merged);
        assert!(data.stack[0].closed_at.is_none());
        assert_eq!(data.stack[1].bookmark_name, "profile");
    }

    #[test]
    fn test_parse_comment_data_missing() {
        assert!(parse_comment_data("no data here").is_none());
    }

    #[test]
    fn test_find_stack_comment_by_sentinel() {
        let comments = vec![
            IssueComment {
                id: 1,
                body: Some("unrelated comment".to_string()),
            },
            IssueComment {
                id: 2,
                body: Some(format!("{SENTINEL}\nstack info")),
            },
        ];
        let found = find_stack_comment(&comments).unwrap();
        assert_eq!(found.id, 2);
    }

    #[test]
    fn test_find_stack_comment_by_legacy_footer() {
        let comments = vec![IssueComment {
            id: 5,
            body: Some(format!(
                "stack\n{LEGACY_FOOTER}(https://github.com/keanemind/jj-stack)*"
            )),
        }];
        let found = find_stack_comment(&comments).unwrap();
        assert_eq!(found.id, 5);
    }

    #[test]
    fn test_find_stack_comment_none() {
        let comments = vec![IssueComment {
            id: 1,
            body: Some("nothing relevant".to_string()),
        }];
        assert!(find_stack_comment(&comments).is_none());
    }

    #[test]
    fn test_bookmark_name_with_markdown_chars() {
        let entries = vec![StackEntry {
            bookmark_name: "[evil](https://evil.com)".to_string(),
            pr_url: Some("https://github.com/o/r/pull/1".to_string()),
            pr_number: Some(1),
            is_current: false,
            is_merged: false,
            closed_at: None,
        }];
        let body = generate_comment_body(&entries, &[]);
        assert!(body.contains("1. [`[evil](https://evil.com)`](https://github.com/o/r/pull/1)\n"));
        assert!(!body.contains("](https://evil.com)\""));
    }

    #[test]
    fn test_new_comments_use_jjpr_data_prefix() {
        let body = generate_comment_body(&sample_live(), &[]);
        assert!(body.contains("JJPR_DATA"), "should use JJPR_DATA prefix");
        assert!(
            !body.contains("STACKER_DATA"),
            "should not use old STACKER_DATA prefix"
        );
    }

    #[test]
    fn test_parse_legacy_stacker_data() {
        let data = StackCommentData {
            version: 0,
            stack: vec![StackCommentItem {
                bookmark_name: "old-bookmark".to_string(),
                pr_url: "https://github.com/o/r/pull/1".to_string(),
                pr_number: 1,
                is_merged: false,
                closed_at: None,
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        let encoded = BASE64.encode(json.as_bytes());
        let old_body = format!("<!--- STACKER_DATA: {encoded} --->");

        let parsed = parse_comment_data(&old_body).expect("should parse legacy format");
        assert_eq!(parsed.stack[0].bookmark_name, "old-bookmark");
    }

    #[test]
    fn test_backward_compat_missing_is_merged() {
        let json = r#"{"version":0,"stack":[{"bookmark_name":"feat","pr_url":"https://github.com/o/r/pull/1","pr_number":1}]}"#;
        let encoded = BASE64.encode(json.as_bytes());
        let body = format!("<!--- JJPR_DATA: {encoded} --->");

        let parsed = parse_comment_data(&body).expect("should parse old format");
        assert!(!parsed.stack[0].is_merged, "missing is_merged should default to false");
        assert!(parsed.stack[0].closed_at.is_none(), "missing closed_at should default to None");
    }

    #[test]
    fn test_backward_compat_missing_closed_at() {
        // Existing payloads in the wild lack closed_at; must parse without it.
        let json = r#"{"version":1,"stack":[{"bookmark_name":"feat","pr_url":"https://github.com/o/r/pull/1","pr_number":1,"is_merged":true}]}"#;
        let encoded = BASE64.encode(json.as_bytes());
        let body = format!("<!--- JJPR_DATA: {encoded} --->");

        let parsed = parse_comment_data(&body).expect("should parse old format");
        assert!(parsed.stack[0].closed_at.is_none());
        assert!(parsed.stack[0].is_merged);
    }

    #[test]
    fn test_closed_at_roundtrips() {
        let live = vec![live_entry("top", 5, true)];
        let fossils = vec![fossil_entry("old", 1, "2026-04-30T12:34:56Z")];
        let body = generate_comment_body(&live, &fossils);
        let data = parse_comment_data(&body).unwrap();
        let item = data
            .stack
            .iter()
            .find(|i| i.bookmark_name == "old")
            .unwrap();
        assert_eq!(item.closed_at.as_deref(), Some("2026-04-30T12:34:56Z"));
    }

    #[test]
    fn test_is_merged_roundtrips() {
        let live = vec![live_entry("profile", 2, false)];
        let fossils = vec![fossil_entry("auth", 1, "2026-01-01T00:00:00Z")];
        let body = generate_comment_body(&live, &fossils);
        let data = parse_comment_data(&body).unwrap();
        // Live entry first, fossil second.
        let by_name: std::collections::HashMap<_, _> = data
            .stack
            .iter()
            .map(|i| (i.bookmark_name.as_str(), i))
            .collect();
        assert!(!by_name["profile"].is_merged);
        assert!(by_name["auth"].is_merged);
    }

    // --- DescriptionNav tests ---

    #[test]
    fn test_description_nav_extract_section() {
        let body = format!(
            "PR description\n\n{}\ncontent here\n{}\n\nuser notes",
            NAV_START, NAV_END
        );
        let section = DescriptionNav::extract_section(&body).unwrap();
        assert!(section.starts_with(NAV_START));
        assert!(section.ends_with(NAV_END));
        assert!(section.contains("content here"));
    }

    #[test]
    fn test_description_nav_extract_section_missing() {
        assert!(DescriptionNav::extract_section("no nav here").is_none());
    }

    #[test]
    fn test_description_nav_splice_appends_when_absent() {
        let body = "PR description";
        let result = DescriptionNav::splice_section(body, "NEW NAV");
        assert!(result.starts_with("PR description"));
        assert!(result.contains("NEW NAV"));
    }

    #[test]
    fn test_description_nav_splice_replaces_when_present() {
        let body = format!(
            "before\n\n{}old nav{}\n\nafter",
            NAV_START, NAV_END
        );
        let new_section = format!("{NAV_START}new nav{NAV_END}");
        let result = DescriptionNav::splice_section(&body, &new_section);
        assert!(result.contains("before"));
        assert!(result.contains("after"));
        assert!(result.contains("new nav"));
        assert!(!result.contains("old nav"));
    }

    #[test]
    fn test_description_nav_roundtrip() {
        let live = sample_live();
        let content = generate_comment_body(&live, &[]);
        let section = DescriptionNav::wrap_section(&content);

        let data = parse_comment_data(&section).unwrap();
        assert_eq!(data.stack.len(), 2);
        assert_eq!(data.stack[0].bookmark_name, "auth");
    }

    #[test]
    fn test_description_nav_preserves_description_sentinels() {
        let body = "<!-- jjpr:description -->\ncommit body\n<!-- /jjpr:description -->\n\nuser notes";
        let section = DescriptionNav::wrap_section("stack nav content\n");
        let result = DescriptionNav::splice_section(body, &section);
        assert!(result.contains("<!-- jjpr:description -->"));
        assert!(result.contains("commit body"));
        assert!(result.contains("<!-- /jjpr:description -->"));
        assert!(result.contains("user notes"));
        assert!(result.contains("stack nav content"));
    }
}
