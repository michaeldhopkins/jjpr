//! The forge requests only `jjpr undo` makes: read one PR, close or reopen
//! it, turn it back into a draft, withdraw review requests, read a branch.
//! One function per forge, which each backend's [`super::Forge`] method calls.

use anyhow::{Context, Result};

use super::http::{ForgeClient, HttpError, url_encode};
use super::types::{PrState, PullRequest};

/// A branch name for a URL path, keeping its slashes: GitHub's `git/ref`
/// and Forgejo's `branches` routes take them as they are.
fn branch_path(branch: &str) -> String {
    url_encode(branch).replace("%2F", "/")
}

fn not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpError>()
        .is_some_and(|h| h.status == 404)
}

/// `Ok(None)` for a 404, the value otherwise.
fn found(result: Result<serde_json::Value>) -> Result<Option<serde_json::Value>> {
    match result {
        Ok(v) => Ok(Some(v)),
        Err(e) if not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// How a forge with no draft field marks a draft in a PR's title.
pub(super) struct TitleDraft {
    /// What it reads as a draft marker, lowercase.
    reads: &'static [&'static str],
    /// What jjpr adds.
    adds: &'static str,
    /// Whether the forge edits a PR with PUT (GitLab) or PATCH (Forgejo).
    put: bool,
}

/// GitLab: `Draft:`, `[Draft]` or `(Draft)`. Its API has no draft field to
/// set, and answers 400 to one.
const GITLAB_DRAFT: TitleDraft = TitleDraft {
    reads: &["draft:", "[draft]", "(draft)"],
    adds: "Draft:",
    put: true,
};

/// Forgejo: `WIP:` or `[WIP]` by default (`WORK_IN_PROGRESS_PREFIXES`). It
/// ignores a draft field.
const FORGEJO_DRAFT: TitleDraft = TitleDraft {
    reads: &["wip:", "[wip]"],
    adds: "WIP:",
    put: false,
};

/// `title` without a marker in `reads`, or `None` when it has none.
fn undrafted<'a>(title: &'a str, reads: &[&str]) -> Option<&'a str> {
    let lower = title.to_ascii_lowercase();
    reads
        .iter()
        .find(|m| lower.starts_with(*m))
        .map(|m| title[m.len()..].trim_start())
}

/// Make the PR at `path` a draft (`draft`) or ready, by its title. A title
/// already as asked is left alone.
fn retitle(c: &ForgeClient, path: &str, how: &TitleDraft, draft: bool) -> Result<()> {
    let title = c.get(path)?["title"].as_str().unwrap_or("").to_string();
    let new = match (draft, undrafted(&title, how.reads)) {
        (true, Some(_)) | (false, None) => return Ok(()),
        (true, None) => format!("{} {title}", how.adds),
        (false, Some(rest)) => rest.to_string(),
    };
    let body = serde_json::json!({ "title": new });
    if how.put {
        c.put(path, &body)?;
    } else {
        c.patch(path, &body)?;
    }
    Ok(())
}

fn sha_at(v: &serde_json::Value, pointer: &str) -> Result<String> {
    v.pointer(pointer)
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("branch response has no commit"))
}

pub(super) mod github {
    use super::*;

    pub fn state(v: &serde_json::Value) -> PrState {
        PrState {
            merged: v["merged_at"].is_string(),
            state: v["state"].as_str().unwrap_or("unknown").to_string(),
        }
    }

    pub fn get_pr(
        c: &ForgeClient,
        owner: &str,
        repo: &str,
        n: u64,
    ) -> Result<(PullRequest, PrState)> {
        let v = c.get(&format!("repos/{owner}/{repo}/pulls/{n}"))?;
        let state = state(&v);
        let pr = serde_json::from_value(v).context("failed to parse PR response")?;
        Ok((pr, state))
    }

    /// Also Forgejo's: the same path and body.
    pub fn set_state(c: &ForgeClient, owner: &str, repo: &str, n: u64, state: &str) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{n}");
        c.patch(&path, &serde_json::json!({ "state": state }))?;
        Ok(())
    }

    /// REST can neither mark a PR ready nor make it a draft again; GraphQL
    /// can, given the PR's node id.
    pub fn set_draft(c: &ForgeClient, owner: &str, repo: &str, n: u64, draft: bool) -> Result<()> {
        let pr = c.get(&format!("repos/{owner}/{repo}/pulls/{n}"))?;
        let node_id = pr["node_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("PR response missing node_id field"))?;
        let mutation = if draft {
            "convertPullRequestToDraft"
        } else {
            "markPullRequestReadyForReview"
        };
        let query = format!(
            "mutation($id: ID!) {{ {mutation}(input: {{ pullRequestId: $id }}) {{ clientMutationId }} }}"
        );
        c.graphql("graphql", &query, &serde_json::json!({ "id": node_id }))?;
        Ok(())
    }

    /// Request reviews (`add`) or withdraw the requests. Also Forgejo's: the
    /// same path and body.
    pub fn reviewers(
        c: &ForgeClient,
        (owner, repo, n): (&str, &str, u64),
        who: &[String],
        add: bool,
    ) -> Result<()> {
        if who.is_empty() {
            return Ok(());
        }
        let path = format!("repos/{owner}/{repo}/pulls/{n}/requested_reviewers");
        let body = serde_json::json!({ "reviewers": who });
        if add {
            c.post(&path, &body)?;
        } else {
            c.delete_with_body(&path, &body)?;
        }
        Ok(())
    }

    pub fn branch_head(
        c: &ForgeClient,
        owner: &str,
        repo: &str,
        b: &str,
    ) -> Result<Option<String>> {
        let path = format!("repos/{owner}/{repo}/git/ref/heads/{}", branch_path(b));
        found(c.get(&path))?
            .map(|v| sha_at(&v, "/object/sha"))
            .transpose()
    }
}

pub(super) mod gitlab {
    use super::*;

    /// An MR: owner, repo and iid.
    pub type Mr<'a> = (&'a str, &'a str, u64);

    /// GitLab names a project by its URL-encoded path.
    pub fn project(owner: &str, repo: &str) -> String {
        format!("{owner}/{repo}").replace('/', "%2F")
    }

    fn mr_path((owner, repo, n): Mr) -> String {
        format!("projects/{}/merge_requests/{n}", project(owner, repo))
    }

    pub fn get_pr(c: &ForgeClient, mr: Mr) -> Result<(PullRequest, PrState)> {
        let v = c.get(&mr_path(mr))?;
        let state = v["state"].as_str().unwrap_or("unknown").to_string();
        let merged = state == "merged";
        Ok((
            super::super::gitlab::parse_mr(&v)?,
            PrState { merged, state },
        ))
    }

    /// `event` is `close` or `reopen`.
    pub fn state_event(c: &ForgeClient, mr: Mr, event: &str) -> Result<()> {
        c.put(&mr_path(mr), &serde_json::json!({ "state_event": event }))?;
        Ok(())
    }

    pub fn set_draft(c: &ForgeClient, mr: Mr, draft: bool) -> Result<()> {
        retitle(c, &mr_path(mr), &GITLAB_DRAFT, draft)
    }

    /// GitLab replaces the reviewer list, so this resolves each username to
    /// its user id and sends the whole list.
    pub fn request_reviewers(c: &ForgeClient, mr: Mr, reviewers: &[String]) -> Result<()> {
        if reviewers.is_empty() {
            return Ok(());
        }
        let project = project(mr.0, mr.1);
        // Batch lookup: fetch all project members in one paginated call
        // instead of N individual user lookups.
        let members = c.get_paginated(&format!("projects/{project}/members/all?per_page=100"))?;
        let member_map: std::collections::HashMap<&str, u64> = members
            .iter()
            .filter_map(|m| Some((m["username"].as_str()?, m["id"].as_u64()?)))
            .collect();
        let mut reviewer_ids = Vec::new();
        for username in reviewers {
            if let Some(&id) = member_map.get(username.as_str()) {
                reviewer_ids.push(id);
                continue;
            }
            // Fallback for non-member reviewers (e.g., invited external users)
            let output = c.get(&format!("users?username={}", url_encode(username)))?;
            let users: Vec<serde_json::Value> =
                serde_json::from_value(output).context("failed to parse user lookup response")?;
            let user_id = users
                .first()
                .and_then(|u| u["id"].as_u64())
                .ok_or_else(|| anyhow::anyhow!("user '{username}' not found on GitLab"))?;
            reviewer_ids.push(user_id);
        }
        c.put(
            &mr_path(mr),
            &serde_json::json!({ "reviewer_ids": reviewer_ids }),
        )?;
        Ok(())
    }

    /// GitLab replaces the reviewer list, so this sends the ones to keep.
    pub fn remove_reviewers(c: &ForgeClient, mr: Mr, who: &[String]) -> Result<()> {
        let v = c.get(&mr_path(mr))?;
        let keep: Vec<u64> = v["reviewers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                let name = r["username"].as_str().unwrap_or("");
                !who.iter().any(|w| w.eq_ignore_ascii_case(name))
            })
            .filter_map(|r| r["id"].as_u64())
            .collect();
        c.put(&mr_path(mr), &serde_json::json!({ "reviewer_ids": keep }))?;
        Ok(())
    }

    pub fn branch_head(
        c: &ForgeClient,
        owner: &str,
        repo: &str,
        b: &str,
    ) -> Result<Option<String>> {
        let project = project(owner, repo);
        let path = format!("projects/{project}/repository/branches/{}", url_encode(b));
        found(c.get(&path))?
            .map(|v| sha_at(&v, "/commit/id"))
            .transpose()
    }
}

pub(super) mod forgejo {
    use super::*;

    pub fn get_pr(
        c: &ForgeClient,
        owner: &str,
        repo: &str,
        n: u64,
    ) -> Result<(PullRequest, PrState)> {
        let v = c.get(&format!("repos/{owner}/{repo}/pulls/{n}"))?;
        let state = PrState {
            merged: v["merged"].as_bool().unwrap_or(false),
            state: v["state"].as_str().unwrap_or("unknown").to_string(),
        };
        let pr = serde_json::from_value(v).context("failed to parse PR response")?;
        Ok((pr, state))
    }

    pub fn set_draft(c: &ForgeClient, owner: &str, repo: &str, n: u64, draft: bool) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{n}");
        retitle(c, &path, &FORGEJO_DRAFT, draft)
    }

    pub fn branch_head(
        c: &ForgeClient,
        owner: &str,
        repo: &str,
        b: &str,
    ) -> Result<Option<String>> {
        let path = format!("repos/{owner}/{repo}/branches/{}", branch_path(b));
        found(c.get(&path))?
            .map(|v| sha_at(&v, "/commit/id"))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use crate::forge::test_server::{StubServer, route};
    use crate::forge::{
        AuthScheme, Forge, ForgeClient, ForgejoForge, GitHubForge, GitLabForge, PaginationStyle,
    };

    fn client(server: &StubServer, auth: AuthScheme, pages: PaginationStyle) -> ForgeClient {
        ForgeClient::new(server.base_url(), "tok".to_string(), auth, pages)
    }

    fn github(server: &StubServer) -> GitHubForge {
        GitHubForge::new(client(
            server,
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        ))
    }

    fn gitlab(server: &StubServer) -> GitLabForge {
        GitLabForge::new(client(
            server,
            AuthScheme::PrivateToken,
            PaginationStyle::LinkHeader,
        ))
    }

    fn forgejo(server: &StubServer) -> ForgejoForge {
        let pages = PaginationStyle::PageNumber { limit: 50 };
        ForgejoForge::new(client(server, AuthScheme::Token, pages))
    }

    /// Every request: `"METHOD target"` and its parsed body (`null` for none).
    fn sent(server: &StubServer) -> Vec<(String, serde_json::Value)> {
        server
            .requests()
            .into_iter()
            .map(|r| {
                let body = serde_json::from_str(&r.body).unwrap_or(serde_json::Value::Null);
                (format!("{} {}", r.method, r.target), body)
            })
            .collect()
    }

    const GITHUB_PR: &str = r#"{"number":5,"html_url":"u","title":"T","body":"B","draft":true,
        "state":"closed","merged_at":null,"node_id":"PR_5",
        "base":{"ref":"main"},"head":{"ref":"feat"},"requested_reviewers":[{"login":"alice"}]}"#;

    #[test]
    fn github_get_pr_reads_the_pr_and_its_state() {
        let server = StubServer::start(vec![route("GET", "/repos/o/r/pulls/5", 200, GITHUB_PR)]);
        let (pr, state) = github(&server).get_pr("o", "r", 5).unwrap();
        assert_eq!(pr.base.ref_name, "main");
        assert_eq!(pr.body.as_deref(), Some("B"));
        assert!(pr.draft);
        assert_eq!(pr.requested_reviewers, vec!["alice"]);
        assert_eq!(state.state, "closed");
        assert!(!state.merged);
        assert!(!crate::forge::is_open(&state));
    }

    #[test]
    fn github_and_forgejo_close_and_reopen_by_state() {
        let server = StubServer::start(vec![route("PATCH", "/repos/o/r/pulls/5", 200, "{}")]);
        github(&server).close_pr("o", "r", 5).unwrap();
        github(&server).reopen_pr("o", "r", 5).unwrap();
        forgejo(&server).close_pr("o", "r", 5).unwrap();
        forgejo(&server).reopen_pr("o", "r", 5).unwrap();
        let states: Vec<_> = sent(&server)
            .into_iter()
            .map(|(_, b)| b["state"].clone())
            .collect();
        assert_eq!(states, vec!["closed", "open", "closed", "open"]);
    }

    #[test]
    fn github_drafts_and_readies_through_graphql() {
        let server = StubServer::start(vec![
            route("GET", "/repos/o/r/pulls/5", 200, GITHUB_PR),
            route("POST", "/graphql", 200, r#"{"data":{}}"#),
        ]);
        github(&server).convert_to_draft("o", "r", 5).unwrap();
        github(&server).mark_pr_ready("o", "r", 5).unwrap();
        let posts: Vec<_> = sent(&server)
            .into_iter()
            .filter(|(line, _)| line == "POST /graphql")
            .map(|(_, b)| b)
            .collect();
        assert_eq!(posts.len(), 2);
        assert!(
            posts[0]["query"]
                .as_str()
                .unwrap()
                .contains("convertPullRequestToDraft")
        );
        assert!(
            posts[1]["query"]
                .as_str()
                .unwrap()
                .contains("markPullRequestReadyForReview")
        );
        assert_eq!(posts[0]["variables"]["id"], "PR_5");
    }

    #[test]
    fn github_and_forgejo_withdraw_review_requests_with_a_delete() {
        let path = "/repos/o/r/pulls/5/requested_reviewers";
        let server = StubServer::start(vec![route("DELETE", path, 200, "{}")]);
        let who = vec!["alice".to_string()];
        github(&server).remove_reviewers("o", "r", 5, &who).unwrap();
        forgejo(&server)
            .remove_reviewers("o", "r", 5, &who)
            .unwrap();
        github(&server).remove_reviewers("o", "r", 5, &[]).unwrap();
        let expected = (
            format!("DELETE {path}"),
            serde_json::json!({ "reviewers": ["alice"] }),
        );
        assert_eq!(
            sent(&server),
            vec![expected.clone(), expected],
            "none sent for no one"
        );
    }

    #[test]
    fn github_requests_reviews_with_a_post() {
        let path = "/repos/o/r/pulls/5/requested_reviewers";
        let server = StubServer::start(vec![route("POST", path, 201, "{}")]);
        github(&server)
            .request_reviewers("o", "r", 5, &["bob".to_string()])
            .unwrap();
        assert_eq!(
            sent(&server),
            vec![(
                format!("POST {path}"),
                serde_json::json!({ "reviewers": ["bob"] })
            )]
        );
    }

    #[test]
    fn github_reads_a_branch_head_and_a_missing_branch() {
        let server = StubServer::start(vec![
            route(
                "GET",
                "/repos/o/r/git/ref/heads/feat/x",
                200,
                r#"{"object":{"sha":"abc"}}"#,
            ),
            route(
                "GET",
                "/repos/o/r/git/ref/heads/gone",
                404,
                r#"{"message":"Not Found"}"#,
            ),
            route("GET", "/repos/o/r/git/ref/heads/err", 500, "{}"),
        ]);
        let gh = github(&server);
        assert_eq!(
            gh.get_branch_head("o", "r", "feat/x").unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(gh.get_branch_head("o", "r", "gone").unwrap(), None);
        assert!(
            gh.get_branch_head("o", "r", "err").is_err(),
            "only a 404 means no branch"
        );
    }

    const MR: &str = "/projects/o%2Fr/merge_requests/5";

    fn gitlab_mr(state: &str, title: &str) -> String {
        format!(
            r#"{{"iid":5,"web_url":"u","title":"{title}","description":"D","state":"{state}",
            "target_branch":"main","source_branch":"feat","draft":false,
            "reviewers":[{{"id":11,"username":"alice"}},{{"id":12,"username":"bob"}}]}}"#
        )
    }

    #[test]
    fn gitlab_get_pr_reads_opened_and_merged() {
        let opened = gitlab_mr("opened", "T");
        let server = StubServer::start(vec![route("GET", MR, 200, &opened)]);
        let (pr, state) = gitlab(&server).get_pr("o", "r", 5).unwrap();
        assert_eq!(pr.head.ref_name, "feat");
        assert!(crate::forge::is_open(&state));
        let merged = gitlab_mr("merged", "T");
        let server = StubServer::start(vec![route("GET", MR, 200, &merged)]);
        let state = gitlab(&server).get_pr_state("o", "r", 5).unwrap();
        assert!(state.merged && !crate::forge::is_open(&state));
    }

    #[test]
    fn gitlab_closes_and_reopens_with_a_state_event() {
        let server = StubServer::start(vec![route("PUT", MR, 200, "{}")]);
        gitlab(&server).close_pr("o", "r", 5).unwrap();
        gitlab(&server).reopen_pr("o", "r", 5).unwrap();
        let events: Vec<_> = sent(&server)
            .into_iter()
            .map(|(_, b)| b["state_event"].clone())
            .collect();
        assert_eq!(events, vec!["close", "reopen"]);
    }

    #[test]
    fn gitlab_drafts_by_title_once() {
        let plain = gitlab_mr("opened", "T");
        let server = StubServer::start(vec![
            route("GET", MR, 200, &plain),
            route("PUT", MR, 200, "{}"),
        ]);
        gitlab(&server).convert_to_draft("o", "r", 5).unwrap();
        assert_eq!(
            sent(&server)[1],
            (
                format!("PUT {MR}"),
                serde_json::json!({ "title": "Draft: T" })
            )
        );
        let draft = gitlab_mr("opened", "Draft: T");
        let server = StubServer::start(vec![route("GET", MR, 200, &draft)]);
        gitlab(&server).convert_to_draft("o", "r", 5).unwrap();
        assert_eq!(server.request_lines(), vec![format!("GET {MR}")]);
    }

    #[test]
    fn gitlab_withdraws_review_requests_by_keeping_the_rest() {
        let mr = gitlab_mr("opened", "T");
        let server = StubServer::start(vec![
            route("GET", MR, 200, &mr),
            route("PUT", MR, 200, "{}"),
        ]);
        gitlab(&server)
            .remove_reviewers("o", "r", 5, &["Alice".to_string()])
            .unwrap();
        assert_eq!(
            sent(&server)[1],
            (
                format!("PUT {MR}"),
                serde_json::json!({ "reviewer_ids": [12] })
            )
        );
    }

    #[test]
    fn gitlab_reads_a_branch_head() {
        let server = StubServer::start(vec![
            route(
                "GET",
                "/projects/o%2Fr/repository/branches/feat%2Fx",
                200,
                r#"{"commit":{"id":"abc"}}"#,
            ),
            route("GET", "/projects/o%2Fr/repository/branches/gone", 404, "{}"),
        ]);
        let gl = gitlab(&server);
        assert_eq!(
            gl.get_branch_head("o", "r", "feat/x").unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(gl.get_branch_head("o", "r", "gone").unwrap(), None);
    }

    #[test]
    fn forgejo_get_pr_reads_merged() {
        let body = r#"{"number":5,"html_url":"u","title":"T","body":"B","state":"closed",
            "merged":true,"base":{"ref":"main"},"head":{"ref":"feat"}}"#;
        let server = StubServer::start(vec![route("GET", "/repos/o/r/pulls/5", 200, body)]);
        let (pr, state) = forgejo(&server).get_pr("o", "r", 5).unwrap();
        assert_eq!(pr.number, 5);
        assert!(state.merged);
    }

    #[test]
    fn forgejo_drafts_by_title_once() {
        let path = "/repos/o/r/pulls/5";
        let plain = r#"{"title":"T"}"#;
        let server = StubServer::start(vec![
            route("GET", path, 200, plain),
            route("PATCH", path, 200, "{}"),
        ]);
        forgejo(&server).convert_to_draft("o", "r", 5).unwrap();
        assert_eq!(
            sent(&server)[1],
            (
                format!("PATCH {path}"),
                serde_json::json!({ "title": "WIP: T" })
            )
        );
        let server = StubServer::start(vec![route("GET", path, 200, r#"{"title":"WIP: T"}"#)]);
        forgejo(&server).convert_to_draft("o", "r", 5).unwrap();
        assert_eq!(server.request_lines(), vec![format!("GET {path}")]);
    }

    #[test]
    fn undrafted_drops_only_the_markers_each_forge_reads() {
        use super::{FORGEJO_DRAFT, GITLAB_DRAFT, undrafted};
        let gl = GITLAB_DRAFT.reads;
        assert_eq!(undrafted("Draft: Add x", gl), Some("Add x"));
        assert_eq!(undrafted("draft:Add x", gl), Some("Add x"));
        assert_eq!(undrafted("[Draft] Add x", gl), Some("Add x"));
        assert_eq!(undrafted("(draft) Add x", gl), Some("Add x"));
        assert_eq!(undrafted("WIP: Add x", gl), None, "not GitLab's");
        let fj = FORGEJO_DRAFT.reads;
        assert_eq!(undrafted("WIP: Add x", fj), Some("Add x"));
        assert_eq!(undrafted("[wip] Add x", fj), Some("Add x"));
        assert_eq!(undrafted("Draft: notes", fj), None, "not Forgejo's");
        assert_eq!(undrafted("Add a draft: x", gl), None);
        assert_eq!(undrafted("Drafting docs", gl), None);
    }

    /// GitLab answers 400 to a `draft` field on an edit; ready means the
    /// title loses its marker.
    #[test]
    fn gitlab_marks_ready_by_title_and_leaves_a_ready_title_alone() {
        let draft = gitlab_mr("opened", "[Draft] T");
        let server = StubServer::start(vec![
            route("GET", MR, 200, &draft),
            route("PUT", MR, 200, "{}"),
        ]);
        gitlab(&server).mark_pr_ready("o", "r", 5).unwrap();
        assert_eq!(
            sent(&server)[1],
            (format!("PUT {MR}"), serde_json::json!({ "title": "T" }))
        );
        let ready = gitlab_mr("opened", "T");
        let server = StubServer::start(vec![route("GET", MR, 200, &ready)]);
        gitlab(&server).mark_pr_ready("o", "r", 5).unwrap();
        assert_eq!(server.request_lines(), vec![format!("GET {MR}")]);
    }

    #[test]
    fn forgejo_reads_a_branch_head() {
        let server = StubServer::start(vec![
            route(
                "GET",
                "/repos/o/r/branches/feat/x",
                200,
                r#"{"commit":{"id":"abc"}}"#,
            ),
            route("GET", "/repos/o/r/branches/gone", 404, "{}"),
        ]);
        let fj = forgejo(&server);
        assert_eq!(
            fj.get_branch_head("o", "r", "feat/x").unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(fj.get_branch_head("o", "r", "gone").unwrap(), None);
    }
}
