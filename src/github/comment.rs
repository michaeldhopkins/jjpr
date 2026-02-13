/// Stack navigation comment generation, parsing, and in-place editing.
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

use super::types::IssueComment;

const SENTINEL: &str = "<!-- stacker:stack-info -->";
const FOOTER: &str = "*Created with [stacker](https://github.com/michaeldhopkins/stacker)*";
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
}

/// Entry for rendering the stack comment.
pub struct StackEntry {
    pub bookmark_name: String,
    pub pr_url: Option<String>,
    pub pr_number: Option<u64>,
    pub is_current: bool,
}

/// Generate the body for a stack navigation comment.
pub fn generate_comment_body(entries: &[StackEntry], default_branch: &str) -> String {
    let data = StackCommentData {
        version: 0,
        stack: entries
            .iter()
            .filter_map(|e| {
                Some(StackCommentItem {
                    bookmark_name: e.bookmark_name.clone(),
                    pr_url: e.pr_url.clone()?,
                    pr_number: e.pr_number?,
                })
            })
            .collect(),
    };

    let json = serde_json::to_string(&data).expect("StackCommentData serialization cannot fail");
    let encoded = BASE64.encode(json.as_bytes());

    let mut body = String::new();
    body.push_str(SENTINEL);
    body.push('\n');
    body.push_str(&format!("<!--- STACKER_DATA: {encoded} --->"));
    body.push('\n');
    body.push_str(&format!(
        "This PR is part of a stack of {} bookmarks:\n\n",
        entries.len()
    ));

    body.push_str(&format!("1. `{default_branch}`\n"));
    for entry in entries {
        if entry.is_current {
            body.push_str(&format!("1. **{} <-- this PR**\n", entry.bookmark_name));
        } else if let Some(url) = &entry.pr_url {
            body.push_str(&format!("1. [{}]({})\n", entry.bookmark_name, url));
        } else {
            body.push_str(&format!("1. `{}`\n", entry.bookmark_name));
        }
    }

    body.push_str(&format!("\n---\n{FOOTER}\n"));
    body
}

/// Parse the machine-readable data from an existing stack comment.
pub fn parse_comment_data(body: &str) -> Option<StackCommentData> {
    let prefix = "<!--- STACKER_DATA: ";
    let suffix = " --->";

    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(prefix)
            && let Some(encoded) = rest.strip_suffix(suffix)
        {
            let bytes = BASE64.decode(encoded).ok()?;
            let data: StackCommentData = serde_json::from_slice(&bytes).ok()?;
            return Some(data);
        }
    }
    None
}

/// Find an existing stacker (or legacy jj-stack) comment in a list of comments.
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
            },
            StackEntry {
                bookmark_name: "profile".to_string(),
                pr_url: Some("https://github.com/o/r/pull/2".to_string()),
                pr_number: Some(2),
                is_current: true,
            },
            StackEntry {
                bookmark_name: "settings".to_string(),
                pr_url: None,
                pr_number: None,
                is_current: false,
            },
        ]
    }

    #[test]
    fn test_generate_comment_body_contains_sentinel() {
        let body = generate_comment_body(&sample_entries(), "main");
        assert!(body.contains(SENTINEL));
    }

    #[test]
    fn test_generate_comment_body_contains_footer() {
        let body = generate_comment_body(&sample_entries(), "main");
        assert!(body.contains(FOOTER));
    }

    #[test]
    fn test_generate_comment_body_marks_current_pr() {
        let body = generate_comment_body(&sample_entries(), "main");
        assert!(body.contains("**profile <-- this PR**"));
    }

    #[test]
    fn test_generate_comment_body_links_other_prs() {
        let body = generate_comment_body(&sample_entries(), "main");
        assert!(body.contains("[auth](https://github.com/o/r/pull/1)"));
    }

    #[test]
    fn test_generate_comment_body_shows_unlinked_bookmarks() {
        let body = generate_comment_body(&sample_entries(), "main");
        assert!(body.contains("`settings`"));
    }

    #[test]
    fn test_generate_comment_body_shows_default_branch() {
        let body = generate_comment_body(&sample_entries(), "main");
        assert!(body.contains("`main`"));
    }

    #[test]
    fn test_roundtrip_comment_data() {
        let body = generate_comment_body(&sample_entries(), "main");
        let data = parse_comment_data(&body).expect("should parse embedded data");
        assert_eq!(data.version, 0);
        assert_eq!(data.stack.len(), 2);
        assert_eq!(data.stack[0].bookmark_name, "auth");
        assert_eq!(data.stack[0].pr_number, 1);
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
}
