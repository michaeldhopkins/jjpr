//! Recently merged PRs, for recognizing a stack's merged base by its commit.
//!
//! When the bottom PR of a stack is squash- or rebase-merged and the forge
//! deletes its branch, any fetch deletes the local bookmark too, and nothing
//! left in the repository says those commits merged (issue #10). The forge
//! still records each merged PR's head commit, so submit asks for the most
//! recently updated merged PRs and matches their heads against the commits
//! below the survivor. One page, one request: a PR merged since the last
//! submit is among the newest, and a miss only means no restack, as before.

use anyhow::{Context, Result};

use super::http::ForgeClient;
use super::types::PullRequest;

/// GitHub: closed PRs, newest update first, keeping the merged ones.
pub(super) fn github(client: &ForgeClient, owner: &str, repo: &str) -> Result<Vec<PullRequest>> {
    let path =
        format!("repos/{owner}/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page=100");
    merged_only(client, &path)
}

/// Forgejo and Gitea: as GitHub, under their own sort name and page size.
pub(super) fn forgejo(client: &ForgeClient, owner: &str, repo: &str) -> Result<Vec<PullRequest>> {
    let path = format!("repos/{owner}/{repo}/pulls?state=closed&sort=recentupdate&page=1&limit=50");
    merged_only(client, &path)
}

/// GitLab filters by state on the server. `project` is already URL-encoded.
pub(super) fn gitlab(client: &ForgeClient, project: &str) -> Result<Vec<PullRequest>> {
    let path = format!(
        "projects/{project}/merge_requests?state=merged&order_by=updated_at&sort=desc&per_page=100"
    );
    let mrs = gitlab_mrs(client, &path)?;
    // An MR that does not parse cannot match a commit; it must not hide the rest.
    Ok(mrs
        .iter()
        .filter_map(|mr| super::gitlab::parse_mr(mr).ok())
        .collect())
}

/// GitHub's [`super::Forge::find_merged_pr`]: the merged PR from branch `head`.
pub(super) fn github_by_head(
    client: &ForgeClient,
    owner: &str,
    repo: &str,
    head: &str,
) -> Result<Option<PullRequest>> {
    let head = super::http::url_encode(head);
    let path = format!("repos/{owner}/{repo}/pulls?head={owner}:{head}&state=closed");
    Ok(merged_only(client, &path)?.into_iter().next())
}

/// GitLab's [`super::Forge::find_merged_pr`]. `project` is already URL-encoded.
pub(super) fn gitlab_by_head(
    client: &ForgeClient,
    project: &str,
    head: &str,
) -> Result<Option<PullRequest>> {
    let head = super::http::url_encode(head);
    let path = format!("projects/{project}/merge_requests?source_branch={head}&state=merged");
    let mrs = gitlab_mrs(client, &path)?;
    mrs.first().map(super::gitlab::parse_mr).transpose()
}

fn gitlab_mrs(client: &ForgeClient, path: &str) -> Result<Vec<serde_json::Value>> {
    serde_json::from_value(client.get(path)?).context("failed to parse merged MR list response")
}

fn merged_only(client: &ForgeClient, path: &str) -> Result<Vec<PullRequest>> {
    let prs: Vec<PullRequest> = serde_json::from_value(client.get(path)?)
        .context("failed to parse closed PR list response")?;
    Ok(prs
        .into_iter()
        .filter(|pr| pr.merged_at.is_some())
        .collect())
}

/// The PR in `prs` merged into `trunk` from exactly `commit`. jj prints short
/// commit ids and forges full ones, so the head must start with `commit`. A PR
/// merged into another branch (a stacked PR merged into its parent) does not
/// count: its commits are not in trunk.
pub fn merged_from<'a>(
    prs: &'a [PullRequest],
    commit: &str,
    trunk: &str,
) -> Option<&'a PullRequest> {
    if commit.is_empty() {
        return None;
    }
    prs.iter()
        .find(|pr| pr.base.ref_name == trunk && pr.head.sha.starts_with(commit))
}

/// How to name `pr`'s branch in a message. Forgejo replaces a deleted head
/// branch's name with `refs/pull/<n>/head`, so that falls back to the PR's
/// reference (`#12`, `!12`).
pub fn branch_name(pr: &PullRequest, kind: super::ForgeKind) -> String {
    let name = &pr.head.ref_name;
    if name.is_empty() || name.starts_with("refs/") {
        kind.format_ref(pr.number)
    } else {
        name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::test_server::{StubServer, route};
    use crate::forge::types::PullRequestRef;
    use crate::forge::{AuthScheme, PaginationStyle};

    fn pr(number: u64, base: &str, head_sha: &str) -> PullRequest {
        let side = |r: &str, sha: &str| PullRequestRef {
            ref_name: r.to_string(),
            label: String::new(),
            sha: sha.to_string(),
        };
        PullRequest {
            number,
            html_url: String::new(),
            title: String::new(),
            body: None,
            base: side(base, ""),
            head: side("feat", head_sha),
            draft: false,
            node_id: String::new(),
            merged_at: Some("2026-10-05T00:00:00Z".to_string()),
            requested_reviewers: vec![],
            author: String::new(),
            stack: None,
        }
    }

    #[test]
    fn merged_from_matches_a_short_id_against_the_full_head_on_trunk() {
        let prs = [
            pr(1, "feat-base", "abc123def4567890"),
            pr(2, "main", "abc123def4567890"),
            pr(3, "main", "fff000"),
        ];
        assert_eq!(
            merged_from(&prs, "abc123def456", "main").map(|p| p.number),
            Some(2)
        );
        assert_eq!(
            merged_from(&prs, "fff000", "main").map(|p| p.number),
            Some(3)
        );
        assert!(merged_from(&prs, "abc123def456", "trunk").is_none());
        assert!(merged_from(&prs, "0123", "main").is_none());
        assert!(
            merged_from(&prs, "", "main").is_none(),
            "an empty id matches nothing"
        );
        assert!(merged_from(&[pr(4, "main", "")], "", "main").is_none());
    }

    #[test]
    fn branch_name_falls_back_to_the_pr_reference_for_a_deleted_forgejo_branch() {
        use crate::forge::ForgeKind;
        let mut merged = pr(13, "main", "abc");
        assert_eq!(branch_name(&merged, ForgeKind::GitHub), "feat");
        merged.head.ref_name = "refs/pull/13/head".to_string();
        assert_eq!(branch_name(&merged, ForgeKind::Forgejo), "#13");
        merged.head.ref_name = String::new();
        assert_eq!(branch_name(&merged, ForgeKind::GitLab), "!13");
    }

    fn client(server: &StubServer) -> ForgeClient {
        ForgeClient::new(
            server.base_url(),
            "tok".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        )
    }

    fn pr_json(number: u64, merged: bool) -> serde_json::Value {
        serde_json::json!({
            "number": number, "html_url": "", "title": "t", "body": null,
            "base": {"ref": "main"}, "head": {"ref": "feat/x"},
            "merged_at": if merged { serde_json::json!("2026-10-05T00:00:00Z") } else { serde_json::Value::Null },
        })
    }

    /// Moved here from `github.rs` unchanged in behaviour: a closed PR that
    /// did not merge is passed over for the merged one.
    #[test]
    fn github_by_head_finds_the_merged_pr_from_the_branch() {
        let path = "/repos/o/r/pulls?head=o:feat%2Fx&state=closed";
        let body = serde_json::json!([pr_json(1, false), pr_json(2, true)]);
        let server = StubServer::start(vec![route("GET", path, 200, &body.to_string())]);
        let found = github_by_head(&client(&server), "o", "r", "feat/x").expect("lookup");
        assert_eq!(found.map(|p| p.number), Some(2));
        let none = serde_json::json!([pr_json(1, false)]).to_string();
        let server = StubServer::start(vec![route("GET", path, 200, &none)]);
        let found = github_by_head(&client(&server), "o", "r", "feat/x").expect("lookup");
        assert!(found.is_none());
    }

    /// Moved here from `gitlab.rs`: the server filters by state, and the first
    /// MR is the answer.
    #[test]
    fn gitlab_by_head_reads_the_first_merged_mr() {
        let path = "/projects/o%2Fr/merge_requests?source_branch=feat%2Fx&state=merged";
        let body = r#"[{"iid": 7, "source_branch": "feat/x", "target_branch": "main"}, {}]"#;
        let server = StubServer::start(vec![route("GET", path, 200, body)]);
        let found = gitlab_by_head(&client(&server), "o%2Fr", "feat/x").expect("lookup");
        assert_eq!(found.map(|p| p.number), Some(7));
        let server = StubServer::start(vec![route("GET", path, 200, "[]")]);
        let found = gitlab_by_head(&client(&server), "o%2Fr", "feat/x").expect("lookup");
        assert!(found.is_none());
        assert_eq!(server.request_lines(), vec![format!("GET {path}")]);
    }
}
