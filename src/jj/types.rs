use serde::Deserialize;

/// A single jj log entry, deserialized from jj template JSON output.
#[derive(Debug, Clone, Deserialize)]
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
}

/// A segment where the user has selected exactly one bookmark.
#[derive(Debug, Clone)]
pub struct NarrowedSegment {
    pub bookmark: Bookmark,
    pub changes: Vec<LogEntry>,
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
