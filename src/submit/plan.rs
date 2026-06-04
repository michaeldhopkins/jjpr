use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::forge::types::{PullRequest, RepoInfo};
use crate::forge::{Forge, ForgeKind};
use crate::jj::types::{Bookmark, NarrowedSegment};

/// What needs to happen for a bookmark that doesn't have a PR yet.
#[derive(Debug)]
pub struct BookmarkNeedingPr {
    pub bookmark: Bookmark,
    pub base_branch: String,
    pub title: String,
    pub body: String,
    /// Whether this new PR should receive reviewer requests after
    /// creation. Determined at plan time from SubmitOptions.reviewer_scope.
    pub request_reviewers_on_create: bool,
}

/// What needs to happen for a bookmark whose PR has the wrong base.
#[derive(Debug)]
pub struct BookmarkNeedingBaseUpdate {
    pub bookmark: Bookmark,
    pub pr: PullRequest,
    pub expected_base: String,
}

/// What needs to happen for a bookmark whose PR body's managed section is stale.
#[derive(Debug)]
pub struct BookmarkNeedingBodyUpdate {
    pub bookmark: Bookmark,
    pub pr_number: u64,
    pub new_body: String,
    /// True when the only change is adding a fingerprint to an
    /// already-correct managed section (one-time migration of a PR
    /// created before fingerprinting). The managed prose is unchanged;
    /// execute prints a quieter message so it doesn't look like jjpr is
    /// rewriting descriptions out of nowhere.
    pub seed: bool,
}

/// What needs to happen for a draft PR that should be marked ready.
#[derive(Debug)]
pub struct BookmarkNeedingReady {
    pub bookmark: Bookmark,
    pub pr_number: u64,
}

/// A bookmark whose PR title doesn't match the current commit description.
#[derive(Debug)]
pub struct TitleDrift {
    pub bookmark: Bookmark,
    pub pr_number: u64,
    pub current_title: String,
    pub expected_title: String,
}

/// A bookmark whose PR was already merged/closed on GitHub.
#[derive(Debug)]
pub struct MergedBookmark {
    pub bookmark: Bookmark,
    pub pr_number: u64,
    pub html_url: String,
    /// ISO-8601 timestamp from the forge marking when the PR became
    /// non-open. Used for fossil ordering in the stack-info comment.
    pub merged_at: Option<String>,
}

/// The full submission plan.
#[derive(Debug)]
pub struct SubmissionPlan {
    pub bookmarks_needing_push: Vec<Bookmark>,
    pub bookmarks_needing_pr: Vec<BookmarkNeedingPr>,
    pub bookmarks_needing_base_update: Vec<BookmarkNeedingBaseUpdate>,
    pub bookmarks_needing_body_update: Vec<BookmarkNeedingBodyUpdate>,
    pub bookmarks_needing_ready: Vec<BookmarkNeedingReady>,
    /// Existing PRs that should receive reviewer requests. Already
    /// filtered by the SubmitOptions reviewer_scope at plan time, so
    /// execute can iterate without re-deriving scope.
    pub bookmarks_needing_reviewers: Vec<(Bookmark, u64)>,
    pub bookmarks_with_title_drift: Vec<TitleDrift>,
    pub bookmarks_already_merged: Vec<MergedBookmark>,
    pub existing_prs: HashMap<String, PullRequest>,
    pub remote_name: String,
    pub repo_info: RepoInfo,
    pub forge_kind: ForgeKind,
    pub all_bookmarks: Vec<Bookmark>,
    pub default_branch: String,
    pub draft: bool,
    pub stack_nav: crate::config::StackNavMode,
    /// Carried from SubmitOptions so execute_submission_plan can stay
    /// in sync with plan-time decisions without re-passing them.
    pub reviewers: Vec<String>,
    pub dry_run: bool,
}

impl SubmissionPlan {
    /// Whether this plan has any actions that will modify remote state.
    pub fn has_actions(&self) -> bool {
        !self.bookmarks_needing_push.is_empty()
            || !self.bookmarks_needing_pr.is_empty()
            || !self.bookmarks_needing_base_update.is_empty()
            || !self.bookmarks_needing_body_update.is_empty()
            || !self.bookmarks_needing_ready.is_empty()
            || !self.bookmarks_needing_reviewers.is_empty()
    }
}

const DESCRIPTION_START: &str = "<!-- jjpr:description -->";
const DESCRIPTION_END: &str = "<!-- /jjpr:description -->";
const FINGERPRINT_PREFIX: &str = "<!-- jjpr:body-fp ";
const FINGERPRINT_SUFFIX: &str = " -->";

/// Stable 64-bit FNV-1a hash of the managed body, hex-encoded.
///
/// This fingerprint is written into the PR body and read back by future
/// jjpr runs (possibly a different binary version), so the algorithm must
/// stay byte-for-byte stable forever — that rules out `std`'s
/// `DefaultHasher`, whose output is explicitly not portable. FNV-1a is
/// trivial, dependency-free, and good enough: we only need change
/// detection, not collision resistance against an adversary.
fn body_fingerprint(managed: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in managed.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Render a fingerprint marker line for a managed body.
fn fingerprint_marker(managed: &str) -> String {
    format!("{FINGERPRINT_PREFIX}{}{FINGERPRINT_SUFFIX}", body_fingerprint(managed))
}

/// Extract the fingerprint jjpr last recorded for this PR's managed body,
/// if present. Absence means the PR predates fingerprinting (or the user
/// deleted the marker) — callers treat that as "ownership unknown".
pub fn extract_fingerprint(pr_body: &str) -> Option<&str> {
    let start = pr_body.find(FINGERPRINT_PREFIX)? + FINGERPRINT_PREFIX.len();
    let end = pr_body[start..].find(FINGERPRINT_SUFFIX)? + start;
    Some(pr_body[start..end].trim())
}

/// Recognized git trailer keys (lowercased). A trailing block of these is
/// stripped from the PR body so commit attribution like `Co-authored-by:`
/// doesn't become the PR description — a body that is nothing but trailers
/// reads as a wiped description.
const TRAILER_KEYS: &[&str] = &[
    "co-authored-by",
    "co-developed-by",
    "signed-off-by",
    "helped-by",
    "reviewed-by",
    "acked-by",
    "tested-by",
    "reported-by",
    "suggested-by",
    "change-id",
];

/// Drop a trailing block of git trailers (and the blank lines around it)
/// from a commit body. Only the contiguous run of recognized trailers at
/// the very end is removed; a trailer that appears mid-body, or any
/// non-trailer line, stops the scan and is preserved.
fn strip_trailers(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut end = lines.len();
    while end > 0 {
        let line = lines[end - 1].trim();
        if line.is_empty() {
            end -= 1;
            continue;
        }
        let is_trailer = line.split_once(':').is_some_and(|(key, value)| {
            !value.trim().is_empty()
                && TRAILER_KEYS.contains(&key.trim().to_ascii_lowercase().as_str())
        });
        if is_trailer {
            end -= 1;
        } else {
            break;
        }
    }
    lines[..end].join("\n").trim_end().to_string()
}

/// Derive the PR title and raw body text from the first change in a segment.
fn derive_pr_title_body(segment: &NarrowedSegment) -> (String, String) {
    if let Some(change) = segment.changes.first() {
        let title = change.description_first_line.clone();
        let mut body = strip_trailers(
            change
                .description
                .strip_prefix(&title)
                .unwrap_or("")
                .trim(),
        );

        if !segment.merge_source_names.is_empty() {
            let note = generate_merge_note(&segment.merge_source_names);
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&note);
        }

        (title, body)
    } else {
        (segment.bookmark.name.clone(), String::new())
    }
}

fn generate_merge_note(source_names: &[String]) -> String {
    let formatted: Vec<String> = source_names.iter().map(|n| format!("`{n}`")).collect();
    let sources_text = match formatted.len() {
        1 => formatted[0].clone(),
        2 => format!("{} and {}", formatted[0], formatted[1]),
        _ => {
            let (last, rest) = formatted.split_last().unwrap();
            format!("{}, and {last}", rest.join(", "))
        }
    };
    let plural = if source_names.len() == 1 {
        "that PR is"
    } else {
        "those PRs are"
    };
    format!(
        "**Merge note:** This change also merges {sources_text} in jj. \
         The diff may include changes from {sources_text} until {plural} merged."
    )
}

/// Wrap commit body text in sentinel markers, followed by a fingerprint
/// of that body, for the initial PR body.
pub fn wrap_managed_body(commit_body: &str) -> String {
    format!(
        "{DESCRIPTION_START}\n{commit_body}\n{DESCRIPTION_END}\n{}",
        fingerprint_marker(commit_body)
    )
}

/// Extract the managed section from a PR body, if sentinel markers are present.
pub fn extract_managed_body(pr_body: &str) -> Option<&str> {
    let start_idx = pr_body.find(DESCRIPTION_START)?;
    let content_start = start_idx + DESCRIPTION_START.len();
    let end_idx = pr_body[content_start..].find(DESCRIPTION_END)? + content_start;
    Some(pr_body[content_start..end_idx].trim())
}

/// Drop the fingerprint marker (and the single newline jjpr writes before
/// it) that immediately follows the closing sentinel, returning the rest
/// of the trailing user content. Leaves a legacy body — one with user
/// content but no marker — untouched.
fn strip_leading_fingerprint(after_end: &str) -> &str {
    let candidate = after_end.strip_prefix('\n').unwrap_or(after_end);
    if candidate.starts_with(FINGERPRINT_PREFIX)
        && let Some(rel) = candidate.find(FINGERPRINT_SUFFIX)
    {
        return &candidate[rel + FINGERPRINT_SUFFIX.len()..];
    }
    after_end
}

/// Replace the managed section (and its fingerprint) in a PR body,
/// preserving everything the user added outside the sentinels.
fn replace_managed_body(pr_body: &str, new_commit_body: &str) -> String {
    let Some(start_idx) = pr_body.find(DESCRIPTION_START) else {
        return pr_body.to_string();
    };
    let Some(end_tag_start) = pr_body[start_idx..].find(DESCRIPTION_END) else {
        return pr_body.to_string();
    };
    let end_idx = start_idx + end_tag_start + DESCRIPTION_END.len();

    let before = &pr_body[..start_idx];
    let after = strip_leading_fingerprint(&pr_body[end_idx..]);
    format!(
        "{before}{DESCRIPTION_START}\n{new_commit_body}\n{DESCRIPTION_END}\n{}{after}",
        fingerprint_marker(new_commit_body)
    )
}

/// What to do with a PR's managed description section, decided by a
/// three-way comparison between the commit-derived body ("ours"), the
/// current PR body ("theirs"), and the fingerprint jjpr last wrote
/// ("base"). The fingerprint is what lets us tell "the commit changed, so
/// the PR is stale" apart from "the user hand-edited the description" —
/// the two are otherwise indistinguishable, and guessing wrong destroys
/// the user's text.
#[derive(Debug, PartialEq, Eq)]
enum BodyReconcile {
    /// PR already matches the commit (or differs only in ways jjpr owns).
    InSync,
    /// Rewrite the managed section. `seed` means the prose is unchanged
    /// and we're only backfilling a missing fingerprint.
    Update { seed: bool },
    /// The user customized the description and the commit hasn't moved —
    /// respect their edit silently.
    Leave,
    /// Both the commit and the PR body changed, or the PR predates
    /// fingerprinting and has drifted: we can't safely pick a winner, so
    /// leave the PR untouched. (A later change surfaces this to the user.)
    Conflict,
}

fn reconcile_body(stored_fp: Option<&str>, current_managed: &str, expected: &str) -> BodyReconcile {
    if current_managed == expected {
        // In sync. Backfill a fingerprint if this PR predates the feature
        // so future commit edits can propagate without hitting Conflict.
        return match stored_fp {
            Some(_) => BodyReconcile::InSync,
            None => BodyReconcile::Update { seed: true },
        };
    }

    let Some(base) = stored_fp else {
        // No fingerprint and the body diverges from the commit. We can't
        // prove jjpr wrote the current text, so we never overwrite it —
        // this is exactly the case that used to silently wipe
        // hand-written descriptions.
        return BodyReconcile::Conflict;
    };

    let pr_edited = body_fingerprint(current_managed) != base;
    let commit_edited = body_fingerprint(expected) != base;
    match (commit_edited, pr_edited) {
        (true, false) => BodyReconcile::Update { seed: false },
        (false, true) => BodyReconcile::Leave,
        (true, true) => BodyReconcile::Conflict,
        // current_managed != expected yet neither side moved from base is
        // impossible, but treat it as nothing-to-do rather than panic.
        (false, false) => BodyReconcile::InSync,
    }
}

/// How `submit` should treat the draft/ready lifecycle. Mutually
/// exclusive states; encoded as an enum so callers can't accidentally
/// set both `draft` and `ready` at once.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DraftMode {
    /// Create new PRs as ready; leave existing draft PRs as drafts.
    /// jjpr's default if neither --draft nor --ready is passed.
    #[default]
    Default,
    /// Create new PRs as drafts. Existing draft PRs unchanged.
    NewAsDraft,
    /// Mark existing draft PRs as ready. New PRs created as ready.
    MarkExistingReady,
}

/// Options for building and executing a submission plan.
///
/// Carries the full user-facing surface for `jjpr submit` and the
/// submit phase of `jjpr watch`. Both commands construct identical
/// options for matching CLI flags so the underlying primitives can't
/// drift between commands.
pub struct SubmitOptions<'a> {
    pub draft_mode: DraftMode,
    pub reviewers: &'a [String],
    pub reviewer_scope: crate::forge::types::ReviewerScope,
    pub stack_base: Option<&'a str>,
    pub stack_nav: crate::config::StackNavMode,
    pub dry_run: bool,
}

/// Build a submission plan by comparing local state with forge state.
pub fn create_submission_plan(
    github: &dyn Forge,
    segments: &[NarrowedSegment],
    remote_name: &str,
    repo_info: &RepoInfo,
    forge_kind: ForgeKind,
    default_branch: &str,
    opts: &SubmitOptions<'_>,
) -> Result<SubmissionPlan> {
    let draft = matches!(opts.draft_mode, DraftMode::NewAsDraft);
    let ready = matches!(opts.draft_mode, DraftMode::MarkExistingReady);
    let reviewers = opts.reviewers;
    let reviewer_scope = opts.reviewer_scope;
    let stack_base = opts.stack_base;
    let stack_nav = opts.stack_nav;
    let dry_run = opts.dry_run;
    // Batch: one API call for all open PRs instead of one per bookmark
    let all_open_prs = github
        .list_open_prs(&repo_info.owner, &repo_info.repo)
        .context("failed to list open PRs (try `jjpr auth test`)")?;

    let pr_map = crate::forge::build_pr_map(all_open_prs, &repo_info.owner);

    let mut bookmarks_needing_push = Vec::new();
    let mut bookmarks_needing_pr = Vec::new();
    let mut bookmarks_needing_base_update = Vec::new();
    let mut bookmarks_needing_body_update = Vec::new();
    let mut bookmarks_needing_ready = Vec::new();
    let mut bookmarks_needing_reviewers = Vec::new();
    let mut bookmarks_with_title_drift = Vec::new();
    let mut bookmarks_already_merged = Vec::new();
    let mut existing_prs: HashMap<String, PullRequest> = HashMap::new();
    let mut all_bookmarks = Vec::new();

    // Tracks bookmark names of segments that will have a PR after this
    // plan executes. Used to apply reviewer_scope filtering at the end:
    // bottom-most, leaf-most, or all live bookmarks get reviewers.
    let mut live_bookmarks_in_order: Vec<String> = Vec::new();

    // Track the effective base: starts at the stack base (or default branch),
    // advances to each live segment's bookmark name. Merged segments don't
    // advance it — their branches are deleted by the forge after merge.
    let mut effective_base = stack_base.unwrap_or(default_branch).to_string();

    for segment in segments {
        let bookmark = &segment.bookmark;
        all_bookmarks.push(bookmark.clone());

        let base_branch = effective_base.clone();

        let existing_pr = pr_map.get(&bookmark.name).cloned();

        if existing_pr.is_none() {
            // No open PR — check if it was already merged before doing anything else
            match github.find_merged_pr(&repo_info.owner, &repo_info.repo, &bookmark.name) {
                Ok(Some(merged_pr)) => {
                    bookmarks_already_merged.push(MergedBookmark {
                        bookmark: bookmark.clone(),
                        pr_number: merged_pr.number,
                        html_url: merged_pr.html_url,
                        merged_at: merged_pr.merged_at,
                    });
                    // Don't advance effective_base — this branch is deleted
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "  Warning: could not check merged status for '{}': {e}",
                        bookmark.name
                    );
                }
                Ok(None) => {}
            }
        }

        // This segment is live — it becomes the base for the next segment
        effective_base = bookmark.name.clone();

        // Skip segments where all commits are empty (e.g., after jj squash).
        // Pushing an empty bookmark would make the PR's diff zero, and GitHub
        // auto-closes PRs when head is no longer ahead of base.
        if segment.changes.iter().all(|c| c.empty) {
            if existing_pr.is_some() {
                eprintln!(
                    "  Warning: '{}' has no file changes (all commits are empty)",
                    bookmark.name
                );
                eprintln!("    Skipping push to avoid closing the existing PR.");
                eprintln!(
                    "    hint: jj bookmark delete {} && jj git push --deleted",
                    bookmark.name
                );
            }
            continue;
        }

        // This segment will have a PR (existing or new). Track it for
        // reviewer_scope filtering after the loop.
        live_bookmarks_in_order.push(bookmark.name.clone());

        // Check if bookmark needs push (after merged check to avoid recreating deleted branches)
        if !bookmark.is_synced {
            bookmarks_needing_push.push(bookmark.clone());
        }

        if let Some(pr) = existing_pr {
            // Check if base needs updating
            if pr.base.ref_name != base_branch {
                bookmarks_needing_base_update.push(BookmarkNeedingBaseUpdate {
                    bookmark: bookmark.clone(),
                    pr: pr.clone(),
                    expected_base: base_branch,
                });
            }

            // Reconcile the managed body section against the commit,
            // using the stored fingerprint to avoid clobbering hand edits.
            let (expected_title, expected_body) = derive_pr_title_body(segment);
            let current_body = pr.body.as_deref().unwrap_or("");
            if let Some(current_managed) = extract_managed_body(current_body) {
                let stored_fp = extract_fingerprint(current_body);
                if let BodyReconcile::Update { seed } =
                    reconcile_body(stored_fp, current_managed, &expected_body)
                {
                    let new_full_body = replace_managed_body(current_body, &expected_body);
                    bookmarks_needing_body_update.push(BookmarkNeedingBodyUpdate {
                        bookmark: bookmark.clone(),
                        pr_number: pr.number,
                        new_body: new_full_body,
                        seed,
                    });
                }
            }

            // Check for title drift (only for single-commit segments — multi-commit
            // segments likely have manually curated PR titles)
            if segment.changes.len() == 1 && pr.title != expected_title {
                bookmarks_with_title_drift.push(TitleDrift {
                    bookmark: bookmark.clone(),
                    pr_number: pr.number,
                    current_title: pr.title.clone(),
                    expected_title,
                });
            }

            // Check if draft PR needs to be marked ready
            if ready && pr.draft {
                bookmarks_needing_ready.push(BookmarkNeedingReady {
                    bookmark: bookmark.clone(),
                    pr_number: pr.number,
                });
            }

            // Track reviewers needed on existing PRs. Filtered by
            // reviewer_scope after the main loop, once we know which
            // bookmarks are bottom/leaf.
            if !reviewers.is_empty() {
                bookmarks_needing_reviewers.push((bookmark.clone(), pr.number));
            }

            existing_prs.insert(bookmark.name.clone(), pr);
        } else {
            let (title, body) = derive_pr_title_body(segment);

            bookmarks_needing_pr.push(BookmarkNeedingPr {
                bookmark: bookmark.clone(),
                base_branch,
                title,
                body: wrap_managed_body(&body),
                // Set after the loop, once reviewer_scope can be resolved.
                request_reviewers_on_create: false,
            });
        }
    }

    // Apply reviewer_scope. Filters who actually receives reviewer requests:
    // bottom = first live bookmark, leaf = last live bookmark, all = every
    // live bookmark. Empty reviewers list means no requests regardless.
    let scoped: std::collections::HashSet<String> = if reviewers.is_empty() {
        std::collections::HashSet::new()
    } else {
        match reviewer_scope {
            crate::forge::types::ReviewerScope::Bottom => {
                live_bookmarks_in_order.first().cloned().into_iter().collect()
            }
            crate::forge::types::ReviewerScope::Leaf => {
                live_bookmarks_in_order.last().cloned().into_iter().collect()
            }
            crate::forge::types::ReviewerScope::All => {
                live_bookmarks_in_order.iter().cloned().collect()
            }
        }
    };
    bookmarks_needing_reviewers.retain(|(b, _)| scoped.contains(&b.name));
    for needs_pr in &mut bookmarks_needing_pr {
        if scoped.contains(&needs_pr.bookmark.name) {
            needs_pr.request_reviewers_on_create = true;
        }
    }

    Ok(SubmissionPlan {
        bookmarks_needing_push,
        bookmarks_needing_pr,
        bookmarks_needing_base_update,
        bookmarks_needing_body_update,
        bookmarks_needing_ready,
        bookmarks_needing_reviewers,
        bookmarks_with_title_drift,
        bookmarks_already_merged,
        existing_prs,
        remote_name: remote_name.to_string(),
        repo_info: repo_info.clone(),
        forge_kind,
        all_bookmarks,
        default_branch: default_branch.to_string(),
        draft,
        stack_nav,
        reviewers: reviewers.to_vec(),
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::{ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequestRef, ReviewSummary, ReviewerScope};
    use crate::jj::types::LogEntry;

    struct StubGitHub {
        prs: HashMap<String, PullRequest>,
    }

    impl Forge for StubGitHub {
        fn list_open_prs(
            &self,
            _owner: &str,
            _repo: &str,
        ) -> Result<Vec<PullRequest>> {
            Ok(self.prs.values().cloned().collect())
        }
        fn create_pr(
            &self, _o: &str, _r: &str, _t: &str, _b: &str,
            _h: &str, _ba: &str, _draft: bool,
        ) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn request_reviewers(
            &self, _o: &str, _r: &str, _n: u64, _revs: &[String],
        ) -> Result<()> {
            unimplemented!()
        }
        fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> {
            unimplemented!()
        }
        fn create_comment(
            &self, _o: &str, _r: &str, _i: u64, _b: &str,
        ) -> Result<IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> {
            unimplemented!()
        }
        fn get_authenticated_user(&self) -> Result<String> {
            Ok("testuser".to_string())
        }
        fn find_merged_pr(
            &self, _o: &str, _r: &str, _h: &str,
        ) -> Result<Option<PullRequest>> {
            Ok(None)
        }
        fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
        fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
        fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
        fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
            Ok(PrState { merged: false, state: "open".to_string() })
        }
    }

    fn make_segment(name: &str, synced: bool) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.to_string(),
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                has_remote: synced,
                is_synced: synced,
            },
            changes: vec![LogEntry {
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                author_name: "Test".to_string(),
                author_email: "test@test.com".to_string(),
                description: format!("Add {name}\n\nDetailed description"),
                description_first_line: format!("Add {name}"),
                parents: vec![],
                local_bookmarks: vec![name.to_string()],
                remote_bookmarks: vec![],
                is_working_copy: false,
                conflict: false,
                empty: false,
            }],
            merge_source_names: vec![],
        }
    }

    fn make_pr(name: &str, base: &str) -> PullRequest {
        PullRequest {
            number: 1,
            html_url: "https://github.com/o/r/pull/1".to_string(),
            title: format!("Add {name}"),
            body: Some("Detailed description".to_string()),
            base: PullRequestRef { ref_name: base.to_string(), label: String::new(), sha: String::new() },
            head: PullRequestRef { ref_name: name.to_string(), label: String::new(), sha: String::new() },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
        }
    }

    #[test]
    fn test_plan_new_pr_needed() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_push.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
        assert_eq!(plan.bookmarks_needing_pr[0].title, "Add feature");
        assert_eq!(
            plan.bookmarks_needing_pr[0].body,
            wrap_managed_body("Detailed description")
        );
    }

    #[test]
    fn test_plan_existing_pr_correct_base() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_push.is_empty());
        assert!(plan.bookmarks_needing_pr.is_empty());
        assert!(plan.bookmarks_needing_base_update.is_empty());
        assert_eq!(plan.existing_prs.len(), 1);
    }

    #[test]
    fn test_plan_existing_pr_wrong_base() {
        let gh = StubGitHub {
            prs: HashMap::from([("profile".to_string(), make_pr("profile", "main"))]),
        };
        // Stack: auth -> profile. Profile's base should be "auth", not "main"
        let segments = vec![
            make_segment("auth", true),
            make_segment("profile", true),
        ];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_base_update.len(), 1);
        assert_eq!(
            plan.bookmarks_needing_base_update[0].expected_base,
            "auth"
        );
    }

    #[test]
    fn test_plan_stacked_base_branches() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![
            make_segment("auth", false),
            make_segment("profile", false),
            make_segment("settings", false),
        ];
        let repo = RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
        assert_eq!(plan.bookmarks_needing_pr[1].base_branch, "auth");
        assert_eq!(plan.bookmarks_needing_pr[2].base_branch, "profile");
    }

    #[test]
    fn test_plan_stale_title_does_not_trigger_body_update() {
        let mut pr = make_pr("feature", "main");
        pr.title = "Old title".to_string();
        // Body has sentinels with matching content — no update needed
        pr.body = Some(wrap_managed_body("Detailed description"));

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_plan_detects_title_drift() {
        let mut pr = make_pr("feature", "main");
        pr.title = "Old title".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_with_title_drift.len(), 1);
        assert_eq!(plan.bookmarks_with_title_drift[0].current_title, "Old title");
        assert_eq!(plan.bookmarks_with_title_drift[0].expected_title, "Add feature");
    }

    #[test]
    fn test_plan_tracks_reviewers_for_existing_prs() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };
        let reviewers = ["alice".to_string()];

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &reviewers, reviewer_scope: ReviewerScope::All, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_reviewers.len(), 1);
        assert_eq!(plan.bookmarks_needing_reviewers[0].1, 1); // pr number
    }

    // --- reviewer_scope tests ---
    //
    // Lock the contract that scope filtering happens at plan time and
    // submit and watch get the same answer for the same scope.

    fn three_segment_stack() -> Vec<NarrowedSegment> {
        vec![
            make_segment("auth", false),
            make_segment("profile", false),
            make_segment("settings", false),
        ]
    }

    #[test]
    fn scope_bottom_targets_only_first_live_segment_existing_prs() {
        let gh = StubGitHub {
            prs: HashMap::from([
                ("auth".to_string(), make_pr("auth", "main")),
                ("profile".to_string(), make_pr("profile", "auth")),
                ("settings".to_string(), make_pr("settings", "profile")),
            ]),
        };
        let segments = three_segment_stack();
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };
        let reviewers = ["alice".to_string()];

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions {
                draft_mode: DraftMode::Default, reviewers: &reviewers,
                reviewer_scope: ReviewerScope::Bottom,
                stack_base: None,
                stack_nav: crate::config::StackNavMode::Comment,
                dry_run: false,
            },
        ).unwrap();

        assert_eq!(plan.bookmarks_needing_reviewers.len(), 1, "scope=bottom must pick one existing PR");
        assert_eq!(plan.bookmarks_needing_reviewers[0].0.name, "auth");
    }

    #[test]
    fn scope_leaf_targets_only_topmost_live_segment_existing_prs() {
        let gh = StubGitHub {
            prs: HashMap::from([
                ("auth".to_string(), make_pr("auth", "main")),
                ("profile".to_string(), make_pr("profile", "auth")),
                ("settings".to_string(), make_pr("settings", "profile")),
            ]),
        };
        let segments = three_segment_stack();
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };
        let reviewers = ["alice".to_string()];

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions {
                draft_mode: DraftMode::Default, reviewers: &reviewers,
                reviewer_scope: ReviewerScope::Leaf,
                stack_base: None,
                stack_nav: crate::config::StackNavMode::Comment,
                dry_run: false,
            },
        ).unwrap();

        assert_eq!(plan.bookmarks_needing_reviewers.len(), 1);
        assert_eq!(plan.bookmarks_needing_reviewers[0].0.name, "settings");
    }

    #[test]
    fn scope_all_targets_every_live_segment_existing_prs() {
        let gh = StubGitHub {
            prs: HashMap::from([
                ("auth".to_string(), make_pr("auth", "main")),
                ("profile".to_string(), make_pr("profile", "auth")),
                ("settings".to_string(), make_pr("settings", "profile")),
            ]),
        };
        let segments = three_segment_stack();
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };
        let reviewers = ["alice".to_string()];

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions {
                draft_mode: DraftMode::Default, reviewers: &reviewers,
                reviewer_scope: ReviewerScope::All,
                stack_base: None,
                stack_nav: crate::config::StackNavMode::Comment,
                dry_run: false,
            },
        ).unwrap();

        assert_eq!(plan.bookmarks_needing_reviewers.len(), 3);
        let names: Vec<&str> = plan.bookmarks_needing_reviewers
            .iter().map(|(b, _)| b.name.as_str()).collect();
        assert_eq!(names, vec!["auth", "profile", "settings"]);
    }

    #[test]
    fn scope_marks_request_reviewers_on_create_for_new_prs() {
        // Mixed: bottom has existing PR, top is new.
        let gh = StubGitHub {
            prs: HashMap::from([("auth".to_string(), make_pr("auth", "main"))]),
        };
        let segments = vec![
            make_segment("auth", false),
            make_segment("profile", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };
        let reviewers = ["alice".to_string()];

        // Bottom: only auth (existing) gets request.
        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions {
                draft_mode: DraftMode::Default, reviewers: &reviewers,
                reviewer_scope: ReviewerScope::Bottom,
                stack_base: None,
                stack_nav: crate::config::StackNavMode::Comment,
                dry_run: false,
            },
        ).unwrap();
        assert_eq!(plan.bookmarks_needing_reviewers.len(), 1);
        assert_eq!(plan.bookmarks_needing_reviewers[0].0.name, "auth");
        // The new PR for profile must NOT be flagged for reviewer request.
        let profile = plan.bookmarks_needing_pr.iter()
            .find(|p| p.bookmark.name == "profile").unwrap();
        assert!(!profile.request_reviewers_on_create);

        // Leaf: only profile (the new one) gets the flag, no existing-PR
        // requests since auth isn't the leaf.
        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions {
                draft_mode: DraftMode::Default, reviewers: &reviewers,
                reviewer_scope: ReviewerScope::Leaf,
                stack_base: None,
                stack_nav: crate::config::StackNavMode::Comment,
                dry_run: false,
            },
        ).unwrap();
        assert!(plan.bookmarks_needing_reviewers.is_empty(),
            "leaf scope shouldn't request on auth (it's the bottom)");
        let profile = plan.bookmarks_needing_pr.iter()
            .find(|p| p.bookmark.name == "profile").unwrap();
        assert!(profile.request_reviewers_on_create,
            "leaf scope must flag the new top PR for reviewer request");
    }

    #[test]
    fn scope_skips_already_merged_segments_when_picking_bottom() {
        // bottom (auth) is externally merged. The "live bottom" should be
        // profile, not auth, since auth's branch is gone.
        struct StubWithMerged {
            open_prs: HashMap<String, PullRequest>,
            merged_prs: HashMap<String, PullRequest>,
        }
        impl Forge for StubWithMerged {
            fn list_open_prs(&self, _: &str, _: &str) -> Result<Vec<PullRequest>> {
                Ok(self.open_prs.values().cloned().collect())
            }
            fn find_merged_pr(&self, _: &str, _: &str, head: &str) -> Result<Option<PullRequest>> {
                Ok(self.merged_prs.get(head).cloned())
            }
            fn create_pr(&self, _: &str, _: &str, _: &str, _: &str, _: &str, _: &str, _: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _: &str, _: &str, _: u64) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _: &str, _: &str, _: u64, _: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _: &str, _: &str, _: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _: &str, _: &str, _: u64, _: &str) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".into()) }
            fn merge_pr(&self, _: &str, _: &str, _: u64, _: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _: &str, _: &str, _: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _: &str, _: &str, _: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _: &str, _: &str, _: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _: &str, _: &str, _: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".into() })
            }
        }

        let mut open_prs = HashMap::new();
        open_prs.insert("profile".to_string(), make_pr("profile", "main"));
        open_prs.insert("settings".to_string(), make_pr("settings", "profile"));
        let mut merged_prs = HashMap::new();
        merged_prs.insert("auth".to_string(), {
            let mut pr = make_pr("auth", "main");
            pr.merged_at = Some("2026-05-01T00:00:00Z".to_string());
            pr
        });
        let gh = StubWithMerged { open_prs, merged_prs };
        let segments = three_segment_stack();
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };
        let reviewers = ["alice".to_string()];

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions {
                draft_mode: DraftMode::Default, reviewers: &reviewers,
                reviewer_scope: ReviewerScope::Bottom,
                stack_base: None,
                stack_nav: crate::config::StackNavMode::Comment,
                dry_run: false,
            },
        ).unwrap();

        assert_eq!(plan.bookmarks_needing_reviewers.len(), 1);
        assert_eq!(plan.bookmarks_needing_reviewers[0].0.name, "profile",
            "bottom skips already-merged auth");
    }

    #[test]
    fn empty_reviewers_skips_scope_entirely() {
        let gh = StubGitHub {
            prs: HashMap::from([
                ("auth".to_string(), make_pr("auth", "main")),
                ("profile".to_string(), make_pr("profile", "auth")),
            ]),
        };
        let segments = vec![
            make_segment("auth", false),
            make_segment("profile", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        // Even with scope=All, no reviewers means no requests.
        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions {
                draft_mode: DraftMode::Default, reviewers: &[],
                reviewer_scope: ReviewerScope::All,
                stack_base: None,
                stack_nav: crate::config::StackNavMode::Comment,
                dry_run: false,
            },
        ).unwrap();
        assert!(plan.bookmarks_needing_reviewers.is_empty());
    }

    #[test]
    fn test_plan_detects_stale_managed_body() {
        let mut pr = make_pr("feature", "main");
        pr.body = Some(wrap_managed_body("Old body text"));

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_body_update.len(), 1);
        // The new body should contain the updated managed section
        assert!(extract_managed_body(&plan.bookmarks_needing_body_update[0].new_body)
            .is_some_and(|m| m == "Detailed description"));
    }

    #[test]
    fn test_plan_no_update_when_managed_body_matches() {
        let mut pr = make_pr("feature", "main");
        pr.body = Some(wrap_managed_body("Detailed description"));

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_plan_preserves_user_content_around_sentinels() {
        let mut pr = make_pr("feature", "main");
        let body_with_extras = format!(
            "User notes above\n\n{}\n\n## Screenshots\nSome screenshot",
            wrap_managed_body("Old body")
        );
        pr.body = Some(body_with_extras);

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_body_update.len(), 1);
        let new_body = &plan.bookmarks_needing_body_update[0].new_body;
        assert!(new_body.starts_with("User notes above"));
        assert!(new_body.contains("## Screenshots\nSome screenshot"));
        assert!(extract_managed_body(new_body).is_some_and(|m| m == "Detailed description"));
    }

    #[test]
    fn test_plan_no_update_when_sentinels_removed() {
        let mut pr = make_pr("feature", "main");
        // User completely removed the sentinels from the body
        pr.body = Some("Completely rewritten body with no sentinels".to_string());

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_wrap_managed_body() {
        let wrapped = wrap_managed_body("hello world");
        assert_eq!(
            wrapped,
            format!(
                "<!-- jjpr:description -->\nhello world\n<!-- /jjpr:description -->\n{}",
                fingerprint_marker("hello world")
            )
        );
        // The managed section is still cleanly extractable, and the
        // recorded fingerprint matches the wrapped content.
        assert_eq!(extract_managed_body(&wrapped), Some("hello world"));
        assert_eq!(
            extract_fingerprint(&wrapped),
            Some(body_fingerprint("hello world").as_str())
        );
    }

    #[test]
    fn test_strip_trailers_removes_trailing_attribution() {
        let body = "Real body paragraph.\n\nCo-authored-by: Claude <noreply@anthropic.com>";
        assert_eq!(strip_trailers(body), "Real body paragraph.");
    }

    #[test]
    fn test_strip_trailers_removes_multiple_trailers() {
        let body = "Body.\n\nSigned-off-by: A <a@x>\nCo-authored-by: B <b@x>";
        assert_eq!(strip_trailers(body), "Body.");
    }

    #[test]
    fn test_strip_trailers_body_that_is_only_a_trailer_becomes_empty() {
        assert_eq!(strip_trailers("Co-authored-by: Claude <noreply@anthropic.com>"), "");
    }

    #[test]
    fn test_strip_trailers_keeps_non_trailer_lines() {
        // A colon line whose key isn't a recognized trailer is prose.
        let body = "Body.\n\nNote: keep this line.";
        assert_eq!(strip_trailers(body), "Body.\n\nNote: keep this line.");
    }

    #[test]
    fn test_strip_trailers_keeps_mid_body_trailer() {
        // Only the trailing run is stripped; a trailer followed by prose stays.
        let body = "Signed-off-by: A <a@x>\n\nMore prose after.";
        assert_eq!(strip_trailers(body), body);
    }

    #[test]
    fn test_derive_body_strips_trailer() {
        let segment = NarrowedSegment {
            bookmark: Bookmark {
                name: "feature".to_string(),
                commit_id: "c".to_string(),
                change_id: "ch".to_string(),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![LogEntry {
                commit_id: "c".to_string(),
                change_id: "ch".to_string(),
                author_name: "T".to_string(),
                author_email: "t@t".to_string(),
                description: "Add feature\n\nWhy this matters.\n\nCo-authored-by: Claude <noreply@anthropic.com>".to_string(),
                description_first_line: "Add feature".to_string(),
                parents: vec![],
                local_bookmarks: vec!["feature".to_string()],
                remote_bookmarks: vec![],
                is_working_copy: false,
                conflict: false,
                empty: false,
            }],
            merge_source_names: vec![],
        };
        let (title, body) = derive_pr_title_body(&segment);
        assert_eq!(title, "Add feature");
        assert_eq!(body, "Why this matters.");
    }

    #[test]
    fn test_body_fingerprint_is_stable() {
        // Pinned so a future refactor can't silently change the algorithm
        // (the value is persisted in PR bodies and read by old/new binaries).
        assert_eq!(body_fingerprint(""), "cbf29ce484222325");
        assert_eq!(body_fingerprint("hello world"), body_fingerprint("hello world"));
        assert_ne!(body_fingerprint("a"), body_fingerprint("b"));
    }

    #[test]
    fn test_reconcile_in_sync_with_fingerprint_does_nothing() {
        let fp = body_fingerprint("same");
        assert_eq!(
            reconcile_body(Some(&fp), "same", "same"),
            BodyReconcile::InSync
        );
    }

    #[test]
    fn test_reconcile_in_sync_without_fingerprint_seeds() {
        // Legacy PR whose body already matches the commit: backfill a
        // fingerprint so the next commit edit can propagate cleanly.
        assert_eq!(
            reconcile_body(None, "same", "same"),
            BodyReconcile::Update { seed: true }
        );
    }

    #[test]
    fn test_reconcile_stale_pr_overwrites() {
        // jjpr last wrote "old"; the commit moved to "new"; the PR is
        // untouched (still "old"). Safe to update.
        let base = body_fingerprint("old");
        assert_eq!(
            reconcile_body(Some(&base), "old", "new"),
            BodyReconcile::Update { seed: false }
        );
    }

    #[test]
    fn test_reconcile_user_edited_pr_is_left() {
        // jjpr last wrote "ours"; the commit still derives "ours"; the user
        // changed the PR to "mine". Respect their edit.
        let base = body_fingerprint("ours");
        assert_eq!(
            reconcile_body(Some(&base), "mine", "ours"),
            BodyReconcile::Leave
        );
    }

    #[test]
    fn test_reconcile_both_changed_is_conflict() {
        // jjpr last wrote "base"; both the commit ("new") and the PR
        // ("mine") moved away from it.
        let base = body_fingerprint("base");
        assert_eq!(
            reconcile_body(Some(&base), "mine", "new"),
            BodyReconcile::Conflict
        );
    }

    #[test]
    fn test_reconcile_legacy_drift_never_overwrites() {
        // The bug this whole feature fixes: a hand-written description with
        // no fingerprint must never be clobbered by the commit-derived body.
        assert_eq!(
            reconcile_body(None, "hand-written prose", "Co-authored-by: Claude"),
            BodyReconcile::Conflict
        );
    }

    #[test]
    fn test_plan_does_not_wipe_legacy_handwritten_body() {
        // End-to-end: a PR with sentinels but no fingerprint (pre-feature)
        // whose managed section was hand-edited to prose absent from the
        // commit. jjpr must leave it alone.
        let mut pr = make_pr("feature", "main");
        pr.body = Some(
            "<!-- jjpr:description -->\nHand-written context the commit lacks.\n<!-- /jjpr:description -->".to_string(),
        );

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_plan_seeds_fingerprint_on_in_sync_legacy_pr() {
        // Legacy PR whose body matches the commit: jjpr should record a
        // fingerprint (a seed update) without changing the prose.
        let mut pr = make_pr("feature", "main");
        pr.body = Some(
            "<!-- jjpr:description -->\nDetailed description\n<!-- /jjpr:description -->".to_string(),
        );

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_body_update.len(), 1);
        assert!(plan.bookmarks_needing_body_update[0].seed);
        let new_body = &plan.bookmarks_needing_body_update[0].new_body;
        assert_eq!(extract_managed_body(new_body), Some("Detailed description"));
        assert!(extract_fingerprint(new_body).is_some());
    }

    #[test]
    fn test_plan_leaves_user_edited_fingerprinted_body() {
        // jjpr previously wrote the commit body ("Add feature\n\nDetailed
        // description" -> "Detailed description"), recorded its fingerprint,
        // then the user replaced the managed prose. Commit unchanged.
        let mut pr = make_pr("feature", "main");
        let user_edited = format!(
            "<!-- jjpr:description -->\nMy own description\n<!-- /jjpr:description -->\n{}",
            fingerprint_marker("Detailed description")
        );
        pr.body = Some(user_edited);

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_body_update.is_empty());
    }

    #[test]
    fn test_extract_managed_body() {
        let body = "<!-- jjpr:description -->\nhello world\n<!-- /jjpr:description -->";
        assert_eq!(extract_managed_body(body), Some("hello world"));
    }

    #[test]
    fn test_extract_managed_body_with_surrounding_content() {
        let body = "User text\n\n<!-- jjpr:description -->\nmanaged\n<!-- /jjpr:description -->\n\nMore user text";
        assert_eq!(extract_managed_body(body), Some("managed"));
    }

    #[test]
    fn test_extract_managed_body_no_markers() {
        assert_eq!(extract_managed_body("plain text"), None);
    }

    #[test]
    fn test_extract_managed_body_only_start_marker() {
        let body = "text\n<!-- jjpr:description -->\nsome content but no end marker";
        assert_eq!(extract_managed_body(body), None);
    }

    #[test]
    fn test_replace_managed_body_preserves_surroundings() {
        let body = "Before\n<!-- jjpr:description -->\nold\n<!-- /jjpr:description -->\nAfter";
        let result = replace_managed_body(body, "new content");
        assert_eq!(
            result,
            format!(
                "Before\n<!-- jjpr:description -->\nnew content\n<!-- /jjpr:description -->\n{}\nAfter",
                fingerprint_marker("new content")
            )
        );
        assert_eq!(extract_managed_body(&result), Some("new content"));
    }

    #[test]
    fn test_replace_managed_body_replaces_old_fingerprint() {
        // A second rewrite must update the fingerprint in place, not append
        // a stale duplicate, and must keep trailing user content intact.
        let first = format!(
            "<!-- jjpr:description -->\nold\n<!-- /jjpr:description -->\n{}\n\n![shot](x.png)",
            fingerprint_marker("old")
        );
        let result = replace_managed_body(&first, "fresh");
        assert_eq!(extract_managed_body(&result), Some("fresh"));
        assert_eq!(
            extract_fingerprint(&result),
            Some(body_fingerprint("fresh").as_str())
        );
        // Exactly one fingerprint marker survives.
        assert_eq!(result.matches(FINGERPRINT_PREFIX).count(), 1);
        assert!(result.ends_with("![shot](x.png)"));
    }

    #[test]
    fn test_replace_managed_body_no_markers() {
        let body = "no markers here";
        assert_eq!(replace_managed_body(body, "new"), body);
    }

    #[test]
    fn test_plan_skips_merged_prs() {
        struct GitHubWithMergedPr;

        impl Forge for GitHubWithMergedPr {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, head: &str) -> Result<Option<PullRequest>> {
                if head == "auth" {
                    Ok(Some(PullRequest {
                        number: 99,
                        html_url: "https://github.com/o/r/pull/99".to_string(),
                        title: "Add auth".to_string(),
                        body: None,
                        base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                        head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
                        draft: false,
                        node_id: String::new(),
                        merged_at: Some("2024-01-01T00:00:00Z".to_string()),
                        requested_reviewers: vec![],
                    }))
                } else {
                    Ok(None)
                }
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![
            make_segment("auth", true),
            make_segment("profile", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubWithMergedPr, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        assert_eq!(plan.bookmarks_already_merged.len(), 1);
        assert_eq!(plan.bookmarks_already_merged[0].bookmark.name, "auth");
        assert_eq!(plan.bookmarks_already_merged[0].pr_number, 99);
        // profile should still get a new PR — based on "main", NOT "auth" (deleted branch)
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].bookmark.name, "profile");
        assert_eq!(
            plan.bookmarks_needing_pr[0].base_branch, "main",
            "PR after a merged segment should base on default branch, not the deleted branch"
        );
    }

    #[test]
    fn test_base_skips_consecutive_merged_segments() {
        // auth and profile are both merged; settings should base on "main"
        struct GitHubTwoMerged;
        impl Forge for GitHubTwoMerged {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, head: &str) -> Result<Option<PullRequest>> {
                if head == "auth" || head == "profile" {
                    Ok(Some(PullRequest {
                        number: if head == "auth" { 1 } else { 2 },
                        html_url: format!("https://github.com/o/r/pull/{}", if head == "auth" { 1 } else { 2 }),
                        title: head.to_string(),
                        body: None,
                        base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                        head: PullRequestRef { ref_name: head.to_string(), label: String::new(), sha: String::new() },
                        draft: false,
                        node_id: String::new(),
                        merged_at: Some("2024-01-01T00:00:00Z".to_string()),
                        requested_reviewers: vec![],
                    }))
                } else {
                    Ok(None)
                }
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![
            make_segment("auth", true),
            make_segment("profile", true),
            make_segment("settings", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubTwoMerged, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        assert_eq!(plan.bookmarks_already_merged.len(), 2);
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].bookmark.name, "settings");
        assert_eq!(
            plan.bookmarks_needing_pr[0].base_branch, "main",
            "PR after two merged segments should base on default branch"
        );
    }

    #[test]
    fn test_base_uses_live_segment_after_merged() {
        // auth is merged, profile has a PR (live), settings needs a new PR
        // settings should base on "profile" (the nearest live segment), not "main"
        struct GitHubOneMergedOneLive;
        impl Forge for GitHubOneMergedOneLive {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![PullRequest {
                    number: 2,
                    html_url: "https://github.com/o/r/pull/2".to_string(),
                    title: "profile".to_string(),
                    body: None,
                    base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                    head: PullRequestRef { ref_name: "profile".to_string(), label: "o:profile".to_string(), sha: "sha_profile".to_string() },
                    draft: false,
                    node_id: String::new(),
                    merged_at: None,
                    requested_reviewers: vec![],
                }])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, head: &str) -> Result<Option<PullRequest>> {
                if head == "auth" {
                    Ok(Some(PullRequest {
                        number: 1,
                        html_url: "https://github.com/o/r/pull/1".to_string(),
                        title: "auth".to_string(),
                        body: None,
                        base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                        head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
                        draft: false,
                        node_id: String::new(),
                        merged_at: Some("2024-01-01T00:00:00Z".to_string()),
                        requested_reviewers: vec![],
                    }))
                } else {
                    Ok(None)
                }
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![
            make_segment("auth", true),
            make_segment("profile", true),
            make_segment("settings", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubOneMergedOneLive, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        assert_eq!(plan.bookmarks_already_merged.len(), 1, "auth should be merged");
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].bookmark.name, "settings");
        assert_eq!(
            plan.bookmarks_needing_pr[0].base_branch, "profile",
            "settings should base on profile (nearest live segment), not main"
        );
    }

    #[test]
    fn test_plan_does_not_skip_closed_but_unmerged_prs() {
        struct GitHubWithClosedPr;

        impl Forge for GitHubWithClosedPr {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, _head: &str) -> Result<Option<PullRequest>> {
                // Closed but not merged — merged_at is None, so find_merged_pr returns None
                Ok(None)
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubWithClosedPr, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        // A closed-but-not-merged PR should NOT be treated as merged
        assert!(plan.bookmarks_already_merged.is_empty());
        assert_eq!(plan.bookmarks_needing_pr.len(), 1, "should create a new PR");
    }

    #[test]
    fn test_plan_merged_bookmark_not_pushed() {
        struct GitHubWithMergedPr;

        impl Forge for GitHubWithMergedPr {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn find_merged_pr(&self, _o: &str, _r: &str, head: &str) -> Result<Option<PullRequest>> {
                if head == "auth" {
                    Ok(Some(PullRequest {
                        number: 99,
                        html_url: "https://github.com/o/r/pull/99".to_string(),
                        title: "Add auth".to_string(),
                        body: None,
                        base: PullRequestRef { ref_name: "main".to_string(), label: String::new(), sha: String::new() },
                        head: PullRequestRef { ref_name: "auth".to_string(), label: String::new(), sha: String::new() },
                        draft: false,
                        node_id: String::new(),
                        merged_at: Some("2024-01-01T00:00:00Z".to_string()),
                        requested_reviewers: vec![],
                    }))
                } else {
                    Ok(None)
                }
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { Ok("test".to_string()) }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        // auth is not synced but already merged — should NOT be pushed
        let segments = vec![make_segment("auth", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &GitHubWithMergedPr, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        assert_eq!(plan.bookmarks_already_merged.len(), 1);
        assert!(
            plan.bookmarks_needing_push.is_empty(),
            "merged bookmarks should not be pushed: {:?}",
            plan.bookmarks_needing_push.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_plan_no_title_drift_when_title_matches() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_with_title_drift.is_empty());
    }

    #[test]
    fn test_plan_no_title_drift_for_multi_commit_segment() {
        let mut pr = make_pr("feature", "main");
        pr.title = "Manually curated title".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let mut segment = make_segment("feature", true);
        segment.changes.push(LogEntry {
            commit_id: "c_extra".to_string(),
            change_id: "ch_extra".to_string(),
            author_name: "Test".to_string(),
            author_email: "test@test.com".to_string(),
            description: "Earlier commit".to_string(),
            description_first_line: "Earlier commit".to_string(),
            parents: vec![],
            local_bookmarks: vec![],
            remote_bookmarks: vec![],
            is_working_copy: false,
            conflict: false,
            empty: false,
        });
        let segments = vec![segment];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(
            plan.bookmarks_with_title_drift.is_empty(),
            "multi-commit segments should not report title drift"
        );
    }

    #[test]
    fn test_plan_no_reviewers_tracked_when_empty() {
        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), make_pr("feature", "main"))]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_reviewers.is_empty());
    }

    #[test]
    fn test_plan_identifies_draft_prs_for_ready() {
        let mut pr = make_pr("feature", "main");
        pr.draft = true;
        pr.node_id = "PR_kwDOxyz".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        // With ready=false, no bookmarks_needing_ready
        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert!(plan.bookmarks_needing_ready.is_empty());

        // With ready=true, draft PR is identified
        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::MarkExistingReady, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        assert_eq!(plan.bookmarks_needing_ready.len(), 1);
        assert_eq!(plan.bookmarks_needing_ready[0].pr_number, 1);
    }

    #[test]
    fn test_plan_filters_fork_prs() {
        let mut fork_pr = make_pr("feature", "main");
        fork_pr.head.label = "someone-else:feature".to_string();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), fork_pr)]),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();

        // Fork PR should be filtered out — treated as if no PR exists
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert!(plan.existing_prs.is_empty());
    }

    #[test]
    fn test_plan_accepts_prs_with_empty_label() {
        let mut pr = make_pr("feature", "main");
        pr.head.label = String::new();

        let gh = StubGitHub {
            prs: HashMap::from([("feature".to_string(), pr)]),
        };
        let segments = vec![make_segment("feature", true)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();

        // Empty label (e.g. from test stubs) should pass through the filter
        assert!(plan.bookmarks_needing_pr.is_empty());
        assert_eq!(plan.existing_prs.len(), 1);
    }

    #[test]
    fn test_plan_error_context_on_list_failure() {
        struct FailingGitHub;
        impl Forge for FailingGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                anyhow::bail!("HTTP 401 Unauthorized")
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { unimplemented!() }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> { unimplemented!() }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let err = create_submission_plan(&FailingGitHub, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("jjpr auth test"), "error should hint at auth: {msg}");
    }

    #[test]
    fn test_plan_warns_on_merged_check_failure() {
        struct MergedCheckFailsGitHub;
        impl Forge for MergedCheckFailsGitHub {
            fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
                Ok(vec![])
            }
            fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> { unimplemented!() }
            fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _r2: &[String]) -> Result<()> { unimplemented!() }
            fn list_comments(&self, _o: &str, _r: &str, _i: u64) -> Result<Vec<IssueComment>> { unimplemented!() }
            fn create_comment(&self, _o: &str, _r: &str, _i: u64, _b: &str) -> Result<IssueComment> { unimplemented!() }
            fn update_comment(&self, _o: &str, _r: &str, _id: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> { unimplemented!() }
            fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> { unimplemented!() }
            fn get_authenticated_user(&self) -> Result<String> { unimplemented!() }
            fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> {
                anyhow::bail!("network timeout")
            }
            fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> { unimplemented!() }
            fn get_pr_checks_status(&self, _o: &str, _r: &str, _h: &str) -> Result<ChecksStatus> { unimplemented!() }
            fn get_pr_reviews(&self, _o: &str, _r: &str, _n: u64) -> Result<ReviewSummary> { unimplemented!() }
            fn get_pr_mergeability(&self, _o: &str, _r: &str, _n: u64) -> Result<PrMergeability> { unimplemented!() }
            fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
                Ok(PrState { merged: false, state: "open".to_string() })
            }
        }

        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        // Should succeed (not abort) and plan a PR despite merged check failing
        let plan = create_submission_plan(
            &MergedCheckFailsGitHub, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert!(plan.bookmarks_already_merged.is_empty());
    }

    #[test]
    fn test_plan_uses_stack_base_for_first_pr() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![
            make_segment("auth", false),
            make_segment("profile", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: Some("coworker-feat"), stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "coworker-feat");
        assert_eq!(plan.bookmarks_needing_pr[1].base_branch, "auth");
    }

    #[test]
    fn test_plan_merge_note_in_pr_body() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let mut segment = make_segment("merge-feat", false);
        segment.merge_source_names = vec!["feat-d".to_string()];
        let segments = vec![segment];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        let body = &plan.bookmarks_needing_pr[0].body;
        assert!(body.contains("**Merge note:**"), "body should contain merge note: {body}");
        assert!(body.contains("`feat-d`"), "body should reference the merge source: {body}");
    }

    #[test]
    fn test_plan_no_merge_note_for_linear() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(&gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false }).unwrap();
        let body = &plan.bookmarks_needing_pr[0].body;
        assert!(!body.contains("Merge note"), "linear segment should have no merge note: {body}");
    }

    #[test]
    fn test_plan_merge_note_three_parents() {
        let note = generate_merge_note(&[
            "feat-b".to_string(),
            "feat-c".to_string(),
            "feat-d".to_string(),
        ]);
        assert!(note.contains("`feat-b`, `feat-c`, and `feat-d`"), "should format 3 sources: {note}");
        assert!(note.contains("those PRs are"), "should use plural: {note}");
    }

    #[test]
    fn test_generate_merge_note_single() {
        let note = generate_merge_note(&["feat-x".to_string()]);
        assert!(note.contains("`feat-x`"));
        assert!(note.contains("that PR is"));
    }

    #[test]
    fn test_generate_merge_note_two() {
        let note = generate_merge_note(&["feat-a".to_string(), "feat-b".to_string()]);
        assert!(note.contains("`feat-a` and `feat-b`"));
        assert!(note.contains("those PRs are"));
    }

    #[test]
    fn test_plan_falls_back_to_default_branch() {
        let gh = StubGitHub {
            prs: HashMap::new(),
        };
        let segments = vec![make_segment("feature", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main", &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();
        assert_eq!(plan.bookmarks_needing_pr[0].base_branch, "main");
    }

    fn make_empty_segment(name: &str, synced: bool) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: Bookmark {
                name: name.to_string(),
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                has_remote: synced,
                is_synced: synced,
            },
            changes: vec![LogEntry {
                commit_id: format!("c_{name}"),
                change_id: format!("ch_{name}"),
                author_name: "Test".to_string(),
                author_email: "test@test.com".to_string(),
                description: format!("Add {name}"),
                description_first_line: format!("Add {name}"),
                parents: vec![],
                local_bookmarks: vec![name.to_string()],
                remote_bookmarks: vec![],
                is_working_copy: false,
                conflict: false,
                empty: true,
            }],
            merge_source_names: vec![],
        }
    }

    #[test]
    fn test_plan_skips_empty_segment_with_existing_pr() {
        // An empty bookmark with an existing PR should be skipped to avoid
        // GitHub auto-closing the PR when the branch is force-pushed.
        let gh = StubGitHub {
            prs: HashMap::from([(
                "auth".to_string(),
                PullRequest {
                    number: 1,
                    html_url: "https://github.com/o/r/pull/1".to_string(),
                    title: "Add auth".to_string(),
                    body: None,
                    base: PullRequestRef {
                        ref_name: "main".to_string(),
                        label: String::new(),
                        sha: String::new(),
                    },
                    head: PullRequestRef {
                        ref_name: "auth".to_string(),
                        label: String::new(),
                        sha: "sha_auth".to_string(),
                    },
                    draft: false,
                    node_id: String::new(),
                    merged_at: None,
                    requested_reviewers: vec![],
                },
            )]),
        };
        let segments = vec![make_empty_segment("auth", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        assert!(plan.bookmarks_needing_push.is_empty(), "should not push empty bookmark");
        assert!(plan.bookmarks_needing_pr.is_empty(), "should not create PR for empty bookmark");
        assert!(plan.bookmarks_needing_base_update.is_empty(), "should not update base of empty bookmark");
    }

    #[test]
    fn test_plan_skips_empty_segment_without_pr() {
        // An empty bookmark without a PR should be silently skipped
        let gh = StubGitHub { prs: HashMap::new() };
        let segments = vec![make_empty_segment("auth", false)];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        assert!(plan.bookmarks_needing_push.is_empty());
        assert!(plan.bookmarks_needing_pr.is_empty());
    }

    #[test]
    fn test_plan_empty_segment_advances_effective_base() {
        // Even though an empty segment is skipped, it should still advance the
        // effective base for subsequent segments.
        let gh = StubGitHub { prs: HashMap::new() };
        let segments = vec![
            make_empty_segment("auth", true),
            make_segment("profile", false),
        ];
        let repo = RepoInfo { owner: "o".to_string(), repo: "r".to_string() };

        let plan = create_submission_plan(
            &gh, &segments, "origin", &repo, ForgeKind::GitHub, "main",
            &SubmitOptions { draft_mode: DraftMode::Default, reviewers: &[], reviewer_scope: ReviewerScope::Bottom, stack_base: None, stack_nav: crate::config::StackNavMode::Comment, dry_run: false },
        ).unwrap();

        // profile should base on "auth" (the empty segment), not "main"
        assert_eq!(plan.bookmarks_needing_pr.len(), 1);
        assert_eq!(plan.bookmarks_needing_pr[0].bookmark.name, "profile");
        assert_eq!(
            plan.bookmarks_needing_pr[0].base_branch, "auth",
            "effective_base should advance through empty segments"
        );
    }
}
