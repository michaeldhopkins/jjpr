//! The merged PR's own stack comment. The remaining PRs' comments are rewritten
//! as each one merges; this one is the merged PR's, so it stops claiming the PR
//! is still a live part of the stack.

use std::collections::{HashMap, HashSet};

use crate::forge::comment::{StackCommentItem, StackEntry, StackNav};
use crate::forge::types::PullRequest;
use crate::forge::{Forge, ForgeKind};
use crate::jj::types::NarrowedSegment;

/// `(live, fossils)` for the comment on `this`, which has just merged. The
/// entries that were live keep their stack order, with every merged one
/// struck through and `this` marked; entries merged before stay as history.
pub(super) fn merged_view(
    items: &[StackCommentItem],
    merged_names: &HashSet<&str>,
    this: &str,
) -> (Vec<StackEntry>, Vec<StackEntry>) {
    let mut live = Vec::new();
    let mut fossils = Vec::new();
    for item in items {
        let entry = StackEntry {
            bookmark_name: item.bookmark_name.clone(),
            pr_url: Some(item.pr_url.clone()),
            pr_number: Some(item.pr_number),
            is_current: item.bookmark_name == this,
            is_merged: item.is_merged || merged_names.contains(item.bookmark_name.as_str()),
            closed_at: item.closed_at.clone(),
        };
        if item.is_merged {
            fossils.push(entry);
        } else {
            live.push(entry);
        }
    }
    (live, fossils)
}

/// [`mark_merged`] for the PR of `segments[seg_idx]`, found in the map of PRs
/// open before the merge. Every segment up to it has merged.
pub(super) fn after_merge(
    forge: &dyn Forge,
    nav: &dyn StackNav,
    pr_map: Option<&HashMap<String, PullRequest>>,
    segments: &[NarrowedSegment],
    seg_idx: usize,
    owner_repo: (&str, &str),
    fk: ForgeKind,
) {
    let this = segments[seg_idx].bookmark.name.as_str();
    let merged_names: HashSet<&str> = segments[..=seg_idx]
        .iter()
        .map(|s| s.bookmark.name.as_str())
        .collect();
    let pr = pr_map.and_then(|m| m.get(this));
    mark_merged(forge, nav, pr, &merged_names, this, owner_repo, fk);
}

/// Rewrite the comment on `pr`, which just merged as the bookmark `this`, to
/// show it merged. `merged_names` are the bookmarks merged so far. Cosmetic,
/// so a failure is a warning and stops nothing.
pub(super) fn mark_merged(
    forge: &dyn Forge,
    nav: &dyn StackNav,
    pr: Option<&PullRequest>,
    merged_names: &HashSet<&str>,
    this: &str,
    (owner, repo): (&str, &str),
    fk: ForgeKind,
) {
    let Some(pr) = pr else {
        return;
    };
    let result = nav.update(forge, owner, repo, pr, &|previous| {
        previous.map_or((vec![], vec![]), |data| {
            merged_view(&data.stack, merged_names, this)
        })
    });
    if let Err(e) = result {
        eprintln!(
            "  Warning: could not mark {} merged in its stack nav: {e}",
            fk.format_ref(pr.number)
        );
    }
}

/// Split a stack-info comment's previous payload into `(live, fossils)`
/// after a forge merge. Items in `merged_names` move to fossils;
/// previously-merged items stay as fossils; live items keep their stored
/// metadata. Newly-merged items inherit `closed_at: None` until the next
/// `jjpr submit` queries the forge to populate the real merge timestamp.
///
/// Order matches the input: live entries follow stack position; fossils
/// preserve the original comment's order, which under jjpr's existing
/// rendering means just-merged entries (formerly live, listed first in
/// `data.stack`) naturally appear above older fossils — matching the
/// "most recent first" sort that the next submit will produce.
pub(super) fn partition_after_merge(
    items: &[StackCommentItem],
    merged_names: &HashSet<&str>,
    current_seg_name: &str,
) -> (Vec<StackEntry>, Vec<StackEntry>) {
    let mut live = Vec::new();
    let mut fossils = Vec::new();
    for item in items {
        let is_merged = item.is_merged || merged_names.contains(item.bookmark_name.as_str());
        let entry = StackEntry {
            bookmark_name: item.bookmark_name.clone(),
            pr_url: Some(item.pr_url.clone()),
            pr_number: Some(item.pr_number),
            is_current: item.bookmark_name == current_seg_name && !is_merged,
            is_merged,
            closed_at: item.closed_at.clone(),
        };
        if is_merged {
            fossils.push(entry);
        } else {
            live.push(entry);
        }
    }
    (live, fossils)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::GitHubForge;
    use crate::forge::comment::{CommentNav, DescriptionNav, generate_comment_body};
    use crate::forge::http::{AuthScheme, ForgeClient, PaginationStyle};
    use crate::forge::test_server::{StubServer, route};

    fn entry(name: &str, number: u64, current: bool, merged: bool) -> StackEntry {
        StackEntry {
            bookmark_name: name.to_string(),
            pr_url: Some(format!("https://github.com/o/r/pull/{number}")),
            pr_number: Some(number),
            is_current: current,
            is_merged: merged,
            closed_at: None,
        }
    }

    /// What submit wrote for the middle PR of `a`, `b`, `c` with `old` merged before.
    fn before_merge() -> String {
        generate_comment_body(
            &[
                entry("a", 1, false, false),
                entry("b", 2, true, false),
                entry("c", 3, false, false),
            ],
            &[entry("old", 9, false, true)],
        )
    }

    fn pr(number: u64, body: Option<&str>) -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": number,
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "title": "t",
            "body": body,
            "base": {"ref": "main", "label": "", "sha": ""},
            "head": {"ref": "b", "label": "", "sha": ""},
            "draft": false,
            "node_id": "",
            "merged_at": null,
            "requested_reviewers": [],
            "user": {"login": "me"},
        }))
        .expect("a pull request")
    }

    fn forge(server: &StubServer) -> GitHubForge {
        GitHubForge::new(ForgeClient::new(
            server.base_url(),
            "tok".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        ))
    }

    fn merged(names: &[&'static str]) -> HashSet<&'static str> {
        names.iter().copied().collect()
    }

    #[test]
    fn the_merged_comment_strikes_merged_prs_and_marks_this_one() {
        let data = crate::forge::comment::parse_comment_data(&before_merge()).expect("data");
        let (live, fossils) = merged_view(&data.stack, &merged(&["a", "b"]), "b");
        let body = generate_comment_body(&live, &fossils);

        assert!(body.contains("This PR was part of a stack:\n\n"), "{body}");
        assert!(
            body.contains(
                "1. ~~[`a`](https://github.com/o/r/pull/1)~~\n\
                 1. **`b` <-- this PR, merged**\n\
                 1. [`c`](https://github.com/o/r/pull/3)\n"
            ),
            "{body}"
        );
        assert!(
            body.contains("<summary>1 earlier merged PR</summary>"),
            "{body}"
        );
        assert!(
            body.contains("1. ~~[`old`](https://github.com/o/r/pull/9)~~"),
            "{body}"
        );
    }

    #[test]
    fn the_rewritten_comment_still_reads_back_as_data() {
        let data = crate::forge::comment::parse_comment_data(&before_merge()).expect("data");
        let (live, fossils) = merged_view(&data.stack, &merged(&["a", "b"]), "b");
        let again =
            crate::forge::comment::parse_comment_data(&generate_comment_body(&live, &fossils))
                .expect("data");
        let merged_flags: Vec<(String, bool)> = again
            .stack
            .iter()
            .map(|i| (i.bookmark_name.clone(), i.is_merged))
            .collect();
        assert_eq!(
            merged_flags,
            vec![
                ("a".to_string(), true),
                ("b".to_string(), true),
                ("c".to_string(), false),
                ("old".to_string(), true),
            ]
        );
    }

    #[test]
    fn an_open_pr_comment_is_unchanged_by_the_merged_rendering() {
        let body = before_merge();
        assert!(body.contains("This PR is part of a stack:"), "{body}");
        assert!(body.contains("1. **`b` <-- this PR**\n"), "{body}");
    }

    #[test]
    fn mark_merged_patches_the_merged_prs_own_comment() {
        let old = serde_json::json!([{"id": 77, "body": before_merge()}]).to_string();
        let server = StubServer::start(vec![
            route(
                "GET",
                "/repos/o/r/issues/2/comments?per_page=100",
                200,
                &old,
            ),
            route("PATCH", "/repos/o/r/issues/comments/77", 200, "{}"),
        ]);
        mark_merged(
            &forge(&server),
            &CommentNav,
            Some(&pr(2, None)),
            &merged(&["a", "b"]),
            "b",
            ("o", "r"),
            ForgeKind::GitHub,
        );

        let patch = server
            .requests()
            .into_iter()
            .find(|r| r.method == "PATCH")
            .expect("the comment was rewritten");
        assert_eq!(patch.target, "/repos/o/r/issues/comments/77");
        let sent: serde_json::Value = serde_json::from_str(&patch.body).expect("json");
        let body = sent["body"].as_str().expect("body");
        assert!(body.contains("**`b` <-- this PR, merged**"), "{body}");
    }

    #[test]
    fn mark_merged_does_nothing_without_a_pr_or_a_comment() {
        let server = StubServer::start(vec![route(
            "GET",
            "/repos/o/r/issues/2/comments?per_page=100",
            200,
            "[]",
        )]);
        let f = forge(&server);
        let names = merged(&["b"]);
        mark_merged(
            &f,
            &CommentNav,
            None,
            &names,
            "b",
            ("o", "r"),
            ForgeKind::GitHub,
        );
        mark_merged(
            &f,
            &CommentNav,
            Some(&pr(2, None)),
            &names,
            "b",
            ("o", "r"),
            ForgeKind::GitHub,
        );

        assert!(server.requests().iter().all(|r| r.method == "GET"));
    }

    #[test]
    fn mark_merged_rewrites_a_description_section_too() {
        let section = format!(
            "<!-- jjpr:stack-nav -->\n{}<!-- /jjpr:stack-nav -->",
            before_merge()
        );
        let server = StubServer::start(vec![route("PATCH", "/repos/o/r/pulls/2", 200, "{}")]);
        mark_merged(
            &forge(&server),
            &DescriptionNav,
            Some(&pr(2, Some(&format!("Adds b.\n\n{section}\n")))),
            &merged(&["a", "b"]),
            "b",
            ("o", "r"),
            ForgeKind::GitHub,
        );

        let patch = &server.requests()[0];
        assert!(patch.body.contains("this PR, merged"), "{}", patch.body);
        assert!(patch.body.contains("Adds b."), "{}", patch.body);
    }

    fn segment(name: &str) -> NarrowedSegment {
        NarrowedSegment {
            bookmark: crate::jj::types::Bookmark {
                name: name.to_string(),
                commit_id: String::new(),
                change_id: String::new(),
                has_remote: true,
                is_synced: true,
            },
            changes: vec![],
            merge_source_names: vec![],
        }
    }

    #[test]
    fn after_merge_marks_everything_up_to_the_merged_segment_as_merged() {
        let old = serde_json::json!([{"id": 77, "body": before_merge()}]).to_string();
        let server = StubServer::start(vec![
            route(
                "GET",
                "/repos/o/r/issues/2/comments?per_page=100",
                200,
                &old,
            ),
            route("PATCH", "/repos/o/r/issues/comments/77", 200, "{}"),
        ]);
        let pr_map = HashMap::from([("b".to_string(), pr(2, None))]);
        let segments = [segment("a"), segment("b"), segment("c")];
        after_merge(
            &forge(&server),
            &CommentNav,
            Some(&pr_map),
            &segments,
            1,
            ("o", "r"),
            ForgeKind::GitHub,
        );

        let patch = server
            .requests()
            .into_iter()
            .find(|r| r.method == "PATCH")
            .expect("rewritten");
        let sent: serde_json::Value = serde_json::from_str(&patch.body).expect("json");
        let body = sent["body"].as_str().expect("body");
        assert!(
            body.contains("~~[`a`]"),
            "a merged earlier in the run: {body}"
        );
        assert!(body.contains("this PR, merged"), "{body}");
        assert!(!body.contains("~~[`c`]"), "c has not merged: {body}");
    }

    proptest::proptest! {
        // Nothing is lost or reordered, only previously merged items are history,
        // and every name merged this run reads as merged.
        #[test]
        fn merged_view_keeps_every_item_in_order(
            flags in proptest::collection::vec((proptest::bool::ANY, proptest::bool::ANY), 0..12),
            this in 0usize..12,
        ) {
            let items: Vec<StackCommentItem> = flags
                .iter()
                .enumerate()
                .map(|(i, (was_merged, _))| StackCommentItem {
                    bookmark_name: format!("b{i}"),
                    pr_url: format!("https://x/{i}"),
                    pr_number: i as u64,
                    is_merged: *was_merged,
                    closed_at: None,
                })
                .collect();
            let names: Vec<String> = flags
                .iter()
                .enumerate()
                .filter(|(_, (_, now))| *now)
                .map(|(i, _)| format!("b{i}"))
                .collect();
            let merged_now: HashSet<&str> = names.iter().map(String::as_str).collect();
            let this = format!("b{this}");
            let (live, fossils) = merged_view(&items, &merged_now, &this);

            proptest::prop_assert_eq!(live.len() + fossils.len(), items.len());
            proptest::prop_assert!(fossils.iter().all(|e| e.is_merged));
            let kept: Vec<&str> = live.iter().chain(&fossils).map(|e| e.bookmark_name.as_str()).collect();
            let mut original: Vec<&str> = items.iter().filter(|i| !i.is_merged).map(|i| i.bookmark_name.as_str()).collect();
            original.extend(items.iter().filter(|i| i.is_merged).map(|i| i.bookmark_name.as_str()));
            proptest::prop_assert_eq!(kept, original);
            for e in live.iter().chain(&fossils) {
                proptest::prop_assert_eq!(e.is_current, e.bookmark_name == this);
                if merged_now.contains(e.bookmark_name.as_str()) {
                    proptest::prop_assert!(e.is_merged);
                }
            }
        }
    }

    #[test]
    fn a_failed_rewrite_is_a_warning_not_a_panic() {
        let server = StubServer::start(vec![route(
            "GET",
            "/repos/o/r/issues/2/comments?per_page=100",
            500,
            "boom",
        )]);
        mark_merged(
            &forge(&server),
            &CommentNav,
            Some(&pr(2, None)),
            &merged(&["b"]),
            "b",
            ("o", "r"),
            ForgeKind::GitHub,
        );
    }
}
