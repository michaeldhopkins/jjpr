//! Every recovery-page link jjpr prints names a section that exists.
//!
//! Messages that give up point at `docs/src/recovering.md` by anchor
//! (`jjpr::docs::recovering("...")`). mdBook derives an anchor from a heading's
//! text, so renaming a heading would silently send users to the top of the
//! page. This reads both sides and fails on any anchor with no heading.

use std::path::Path;

/// mdBook's anchor for a heading: lowercase, spaces to hyphens, and every
/// other character that is not alphanumeric, `-` or `_` dropped.
fn anchor(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

fn sources(dir: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(std::fs::read_to_string(&path).unwrap());
        }
    }
}

/// The anchors named in `recovering("...")` calls across `src/`.
fn linked_anchors() -> Vec<String> {
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let marker = "recovering(\"";
    let mut anchors = Vec::new();
    for text in files {
        for (at, _) in text.match_indices(marker) {
            let rest = &text[at + marker.len()..];
            let end = rest.find('"').expect("closing quote");
            anchors.push(rest[..end].to_string());
        }
    }
    anchors
}

#[test]
fn anchor_matches_mdbook() {
    assert_eq!(
        anchor("A merged PR sits under a merge commit"),
        "a-merged-pr-sits-under-a-merge-commit"
    );
    assert_eq!(
        anchor("Merged commits are still in `jj log` after a restack"),
        "merged-commits-are-still-in-jj-log-after-a-restack"
    );
}

#[test]
fn every_recovery_link_names_a_heading_on_the_page() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let page = std::fs::read_to_string(root.join("docs/src/recovering.md")).unwrap();
    let headings: Vec<String> = page
        .lines()
        .filter_map(|l| l.strip_prefix("## "))
        .map(anchor)
        .collect();
    let linked = linked_anchors();
    assert!(
        linked.len() > 1,
        "found {linked:?}; the scan should see the docs module and its callers"
    );
    for a in linked.iter().filter(|a| a.as_str() != "x") {
        assert!(
            headings.contains(a),
            "no heading for #{a}; headings: {headings:?}"
        );
    }
    let summary = std::fs::read_to_string(root.join("docs/src/SUMMARY.md")).unwrap();
    assert!(
        summary.contains("(recovering.md)"),
        "the page is in the book"
    );
}
