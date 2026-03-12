/// Stack navigation comment generation, parsing, and in-place editing.
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

use super::types::IssueComment;

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
}

/// Entry for rendering the stack comment.
pub struct StackEntry {
    pub bookmark_name: String,
    pub pr_url: Option<String>,
    pub pr_number: Option<u64>,
    pub is_current: bool,
    pub is_merged: bool,
}

/// Generate the body for a stack navigation comment.
pub fn generate_comment_body(entries: &[StackEntry]) -> String {
    let data = StackCommentData {
        version: 1,
        stack: entries
            .iter()
            .filter_map(|e| {
                Some(StackCommentItem {
                    bookmark_name: e.bookmark_name.clone(),
                    pr_url: e.pr_url.clone()?,
                    pr_number: e.pr_number?,
                    is_merged: e.is_merged,
                })
            })
            .collect(),
    };

    let json = serde_json::to_string(&data).expect("StackCommentData serialization cannot fail");
    let encoded = BASE64.encode(json.as_bytes());

    let mut body = String::new();
    body.push_str(SENTINEL);
    body.push('\n');
    body.push_str(&format!("<!--- JJPR_DATA: {encoded} --->"));
    body.push('\n');
    body.push_str("This PR is part of a stack:\n\n");

    for entry in entries {
        if entry.is_current {
            body.push_str(&format!("1. **`{}` <-- this PR**\n", entry.bookmark_name));
        } else if entry.is_merged {
            if let Some(url) = &entry.pr_url {
                body.push_str(&format!(
                    "1. ~~[`{}`]({url})~~ :white_check_mark:\n",
                    entry.bookmark_name
                ));
            } else {
                body.push_str(&format!(
                    "1. ~~`{}`~~ :white_check_mark:\n",
                    entry.bookmark_name
                ));
            }
        } else if let Some(url) = &entry.pr_url {
            body.push_str(&format!("1. [`{}`]({url})\n", entry.bookmark_name));
        } else {
            body.push_str(&format!("1. `{}`\n", entry.bookmark_name));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entries() -> Vec<StackEntry> {
        vec![
            StackEntry {
                bookmark_name: "auth".to_string(),
                pr_url: Some("https://github.com/o/r/pull/1".to_string()),
                pr_number: Some(1),
                is_current: false,
                is_merged: false,
            },
            StackEntry {
                bookmark_name: "profile".to_string(),
                pr_url: Some("https://github.com/o/r/pull/2".to_string()),
                pr_number: Some(2),
                is_current: true,
                is_merged: false,
            },
            StackEntry {
                bookmark_name: "settings".to_string(),
                pr_url: None,
                pr_number: None,
                is_current: false,
                is_merged: false,
            },
        ]
    }

    #[test]
    fn test_generate_comment_body_contains_sentinel() {
        let body = generate_comment_body(&sample_entries());
        assert!(body.contains(SENTINEL));
    }

    #[test]
    fn test_generate_comment_body_contains_footer() {
        let body = generate_comment_body(&sample_entries());
        assert!(body.contains(FOOTER));
    }

    #[test]
    fn test_generate_comment_body_marks_current_pr() {
        let body = generate_comment_body(&sample_entries());
        assert!(body.contains("**`profile` <-- this PR**"));
    }

    #[test]
    fn test_generate_comment_body_links_other_prs() {
        let body = generate_comment_body(&sample_entries());
        assert!(body.contains("1. [`auth`](https://github.com/o/r/pull/1)\n"));
    }

    #[test]
    fn test_generate_comment_body_shows_unlinked_bookmarks() {
        let body = generate_comment_body(&sample_entries());
        assert!(body.contains("`settings`"));
    }

    #[test]
    fn test_generate_comment_body_excludes_default_branch() {
        let body = generate_comment_body(&sample_entries());
        // Trunk is the target, not part of the stack
        assert!(!body.contains("1. `main`"));
    }

    #[test]
    fn test_roundtrip_comment_data() {
        let body = generate_comment_body(&sample_entries());
        let data = parse_comment_data(&body).expect("should parse embedded data");
        assert_eq!(data.version, 1);
        assert_eq!(data.stack.len(), 2);
        assert_eq!(data.stack[0].bookmark_name, "auth");
        assert_eq!(data.stack[0].pr_number, 1);
        assert!(!data.stack[0].is_merged);
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
        }];
        let body = generate_comment_body(&entries);
        // Bookmark name is wrapped in backticks inside the link, neutralizing markdown injection
        assert!(body.contains("1. [`[evil](https://evil.com)`](https://github.com/o/r/pull/1)\n"));
        // The evil URL appears only inside backticks (code span), not as a rendered link
        assert!(!body.contains("](https://evil.com)\""));
    }

    #[test]
    fn test_new_comments_use_jjpr_data_prefix() {
        let body = generate_comment_body(&sample_entries());
        assert!(body.contains("JJPR_DATA"), "should use JJPR_DATA prefix");
        assert!(
            !body.contains("STACKER_DATA"),
            "should not use old STACKER_DATA prefix"
        );
    }

    #[test]
    fn test_parse_legacy_stacker_data() {
        // Simulate a comment written by the old version using STACKER_DATA
        let data = StackCommentData {
            version: 0,
            stack: vec![StackCommentItem {
                bookmark_name: "old-bookmark".to_string(),
                pr_url: "https://github.com/o/r/pull/1".to_string(),
                pr_number: 1,
                is_merged: false,
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
        // Old blobs won't have is_merged — should default to false
        let json = r#"{"version":0,"stack":[{"bookmark_name":"feat","pr_url":"https://github.com/o/r/pull/1","pr_number":1}]}"#;
        let encoded = BASE64.encode(json.as_bytes());
        let body = format!("<!--- JJPR_DATA: {encoded} --->");

        let parsed = parse_comment_data(&body).expect("should parse old format");
        assert!(!parsed.stack[0].is_merged, "missing is_merged should default to false");
    }

    #[test]
    fn test_is_merged_roundtrips() {
        let entries = vec![
            StackEntry {
                bookmark_name: "auth".to_string(),
                pr_url: Some("https://github.com/o/r/pull/1".to_string()),
                pr_number: Some(1),
                is_current: false,
                is_merged: true,
            },
            StackEntry {
                bookmark_name: "profile".to_string(),
                pr_url: Some("https://github.com/o/r/pull/2".to_string()),
                pr_number: Some(2),
                is_current: false,
                is_merged: false,
            },
        ];
        let body = generate_comment_body(&entries);
        let data = parse_comment_data(&body).unwrap();
        assert!(data.stack[0].is_merged);
        assert!(!data.stack[1].is_merged);
    }

    #[test]
    fn test_merged_entry_renders_strikethrough() {
        let entries = vec![StackEntry {
            bookmark_name: "auth".to_string(),
            pr_url: Some("https://github.com/o/r/pull/1".to_string()),
            pr_number: Some(1),
            is_current: false,
            is_merged: true,
        }];
        let body = generate_comment_body(&entries);
        assert!(
            body.contains("~~[`auth`](https://github.com/o/r/pull/1)~~ :white_check_mark:"),
            "merged entry should have strikethrough and checkmark: {body}"
        );
    }
}
