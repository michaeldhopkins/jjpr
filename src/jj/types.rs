use serde::Deserialize;

/// A single jj log entry, deserialized from jj template JSON output.
#[derive(Debug, Clone, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct LogEntry {
    pub commit_id: String,
    pub change_id: String,
    pub author_name: String,
    pub author_email: String,
    pub description: String,
    pub description_first_line: String,
    pub parents: Vec<String>,
    pub local_bookmarks: Vec<String>,
    pub remote_bookmarks: Vec<String>,
    pub is_working_copy: bool,
    pub conflict: bool,
    pub empty: bool,
}

/// A bookmark pointing at a specific change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub name: String,
    pub commit_id: String,
    pub change_id: String,
    pub has_remote: bool,
    pub is_synced: bool,
}

/// A group of consecutive changes between two bookmark points (or trunk and a bookmark).
#[derive(Debug, Clone)]
pub struct BookmarkSegment {
    pub bookmarks: Vec<Bookmark>,
    pub changes: Vec<LogEntry>,
    /// For merge commits: bookmark names of parents NOT followed in the linear path.
    /// Empty for non-merge segments.
    pub merge_source_names: Vec<String>,
}

/// A segment where the user has selected exactly one bookmark.
#[derive(Debug, Clone)]
pub struct NarrowedSegment {
    pub bookmark: Bookmark,
    pub changes: Vec<LogEntry>,
    pub merge_source_names: Vec<String>,
}

/// A linear stack of segments from trunk to a leaf bookmark.
#[derive(Debug, Clone)]
pub struct BranchStack {
    pub segments: Vec<BookmarkSegment>,
    /// If the stack is based on a foreign branch (not trunk), this is the branch name.
    pub base_branch: Option<String>,
}

/// A git remote.
#[derive(Debug, Clone)]
pub struct GitRemote {
    pub name: String,
    pub url: String,
}

/// `@` and the commits it sits on, read before and after a fetch to tell whether the
/// fetch rewrote them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingCopyView {
    /// Commit ids of `@`'s parents.
    pub parents: Vec<String>,
    /// Change ids of `@` and its parents.
    pub change_ids: Vec<String>,
}

/// Parse `jj log` lines of `<commit id> <change id> <wc|parent>`.
pub fn parse_working_copy_view(output: &str) -> WorkingCopyView {
    let mut view = WorkingCopyView::default();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(commit), Some(change), Some(role)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        view.change_ids.push(change.to_string());
        if role == "parent" {
            view.parents.push(commit.to_string());
        }
    }
    view
}

#[cfg(test)]
mod working_copy_view_tests {
    use super::*;

    #[test]
    fn view_separates_parents_from_the_working_copy() {
        let view = parse_working_copy_view("c1 ch1 wc\np1 chp1 parent\np2 chp2 parent\n\njunk\n");
        assert_eq!(view.parents, vec!["p1", "p2"]);
        assert_eq!(view.change_ids, vec!["ch1", "chp1", "chp2"]);
    }

    #[test]
    fn empty_output_is_an_empty_view() {
        assert_eq!(parse_working_copy_view(""), WorkingCopyView::default());
    }
}
