use std::collections::HashMap;

use anyhow::{Context, Result};

use super::http::{ForgeClient, HttpError, url_encode};
use super::types::{ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PrStatusBundle, PullRequest, ReviewSummary, Stack};
use super::Forge;

/// GitHub implementation using direct HTTP via `ForgeClient`.
pub struct GitHubForge {
    client: ForgeClient,
}

impl GitHubForge {
    pub fn new(client: ForgeClient) -> Self {
        Self { client }
    }

    /// Classic branch-protection "dismiss stale reviews" via GraphQL.
    /// `Ok(Some(v))` when the branch has a classic protection rule (`v` is its
    /// setting); `Ok(None)` when `branchProtectionRule` is null (ruleset-only or
    /// unprotected — the caller then checks rulesets); `Err` on GraphQL/permission
    /// failure.
    fn classic_protection_dismisses_stale(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
    ) -> Result<Option<bool>> {
        let query = "query($owner:String!,$repo:String!,$ref:String!){ \
             repository(owner:$owner,name:$repo){ ref(qualifiedName:$ref){ \
             branchProtectionRule{ dismissesStaleReviews } } } }";
        let vars = serde_json::json!({
            "owner": owner, "repo": repo, "ref": format!("refs/heads/{base}"),
        });
        let data = self.client.graphql("graphql", query, &vars)?;
        Ok(parse_classic_dismisses(&data))
    }

    /// Effective branch rules (`GET /repos/{o}/{r}/rules/branches/{branch}`),
    /// which include rulesets that the classic GraphQL field omits.
    /// `Ok(Some(true))` if any `pull_request` rule sets
    /// `dismiss_stale_reviews_on_push`; `Ok(Some(false))` if not; `Ok(None)` on a
    /// permission/other error (can't tell).
    fn rules_dismiss_stale(&self, owner: &str, repo: &str, base: &str) -> Result<Option<bool>> {
        let path = format!("repos/{owner}/{repo}/rules/branches/{}", url_encode(base));
        match self.client.get(&path) {
            Ok(value) => Ok(Some(rules_dismiss_stale_on_push(&value))),
            Err(_) => Ok(None),
        }
    }
}

/// Extract `branchProtectionRule.dismissesStaleReviews` from a GraphQL `ref`
/// response. `None` when there's no classic protection rule (the field is null),
/// which the caller treats as "check rulesets instead".
fn parse_classic_dismisses(data: &serde_json::Value) -> Option<bool> {
    data["repository"]["ref"]["branchProtectionRule"]["dismissesStaleReviews"].as_bool()
}

/// Whether the effective-rules array (`GET .../rules/branches/{b}`) contains a
/// `pull_request` rule that dismisses stale reviews on push. Covers rulesets and
/// classic protection alike (GitHub reports both here).
fn rules_dismiss_stale_on_push(rules: &serde_json::Value) -> bool {
    rules.as_array().is_some_and(|rules| {
        rules.iter().any(|r| {
            r["type"] == serde_json::json!("pull_request")
                && r["parameters"]["dismiss_stale_reviews_on_push"].as_bool() == Some(true)
        })
    })
}

/// Parse check-runs and commit status into a `ChecksStatus`.
fn parse_checks_status(
    check_runs: &serde_json::Value,
    commit_status: &serde_json::Value,
) -> ChecksStatus {
    let runs = check_runs["check_runs"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default();
    let statuses = commit_status["statuses"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default();

    if runs.is_empty() && statuses.is_empty() {
        return ChecksStatus::None;
    }

    let mut has_pending = false;
    let mut has_failure = false;

    for run in runs {
        match run["conclusion"].as_str() {
            Some("success") | Some("skipped") | Some("neutral") => {}
            None if run["status"].as_str() == Some("in_progress")
                || run["status"].as_str() == Some("queued") =>
            {
                has_pending = true;
            }
            _ => has_failure = true,
        }
    }

    for s in statuses {
        match s["state"].as_str() {
            Some("success") => {}
            Some("pending") => has_pending = true,
            _ => has_failure = true,
        }
    }

    if has_failure {
        ChecksStatus::Fail
    } else if has_pending {
        ChecksStatus::Pending
    } else {
        ChecksStatus::Pass
    }
}

/// PRs per GraphQL batch query.
///
/// Each PR pulls up to ~200 nodes (100 reviews + 100 check contexts), well
/// under GitHub's 500,000-node ceiling, so this is a latency and
/// error-blast-radius tradeoff rather than a hard limit: one oversized query
/// that trips a node or point limit would strand the whole stack on the slow
/// path, while batches this size still collapse a typical stack to one request.
const GRAPHQL_BATCH_SIZE: usize = 20;

/// The connection page size GitHub allows; asking for more is `EXCESSIVE_PAGINATION`.
const GRAPHQL_PAGE_SIZE: usize = 100;

/// Fields the status view needs, for one PR.
///
/// This asks for the *raw* check and review records rather than GitHub's
/// pre-rolled `statusCheckRollup.state` / `reviewDecision` so the REST parsers
/// below stay the single source of truth for how those roll up. The rollup
/// enums are close to jjpr's own but not identical, and a silent disagreement
/// between the REST and GraphQL paths is worse than a slightly bigger query.
///
/// One deliberate difference from the per-PR path: `commits(last: 1)` resolves
/// the head as it is *now*, whereas the per-PR path pins to the sha carried by
/// the earlier PR-list response. If a PR's head moves between those two calls,
/// this reports the newer commit's checks and the per-PR path reports the older
/// one's. That makes the batch fresher rather than wrong, so it is left alone;
/// pinning would mean threading each PR's oid through a second query root.
const PR_STATUS_FRAGMENT: &str = r"
fragment PrStatus on PullRequest {
  number
  mergeable
  reviews(last: 100) {
    totalCount
    nodes { state author { login } }
  }
  commits(last: 1) {
    nodes {
      commit {
        statusCheckRollup {
          contexts(first: 100) {
            totalCount
            nodes {
              __typename
              ... on CheckRun { conclusion status }
              ... on StatusContext { state }
            }
          }
        }
      }
    }
  }
}
";

/// What a batched GraphQL answer could not fully cover for one PR.
///
/// GraphQL connections cap at 100 per page. Rather than paginate inside the
/// batch (which would serialize the very round trips the batch exists to
/// collapse), an over-100 connection is recorded here and refilled from REST,
/// which paginates without limit.
#[derive(Debug, Default, PartialEq, Eq)]
struct Truncation {
    reviews: bool,
    checks: bool,
}

/// Re-shape GraphQL check contexts into the REST payloads `parse_checks_status`
/// expects, so both paths share one set of precedence rules.
///
/// GraphQL spells its enums in SCREAMING_CASE (`IN_PROGRESS`) where REST uses
/// snake_case (`in_progress`); lowercasing is the whole translation, and it is
/// exact for every value of `CheckRun.conclusion`, `CheckRun.status`, and
/// `StatusContext.state`.
fn contexts_to_rest_shape(contexts: &[serde_json::Value]) -> (serde_json::Value, serde_json::Value) {
    let mut check_runs = Vec::new();
    let mut statuses = Vec::new();

    for context in contexts {
        match context["__typename"].as_str() {
            Some("CheckRun") => check_runs.push(serde_json::json!({
                // A null conclusion is meaningful — it is how an in-flight run
                // is distinguished from a finished one — so it must stay null.
                "conclusion": context["conclusion"].as_str().map(str::to_lowercase),
                "status": context["status"].as_str().map(str::to_lowercase),
            })),
            Some("StatusContext") => statuses.push(serde_json::json!({
                "state": context["state"].as_str().map(str::to_lowercase),
            })),
            _ => {}
        }
    }

    (
        serde_json::json!({ "check_runs": check_runs }),
        serde_json::json!({ "statuses": statuses }),
    )
}

/// Re-shape GraphQL review nodes into the REST payload `parse_review_summary`
/// expects. GraphQL nests the author under `author`, REST under `user`; the
/// state enum spellings are already identical.
fn reviews_to_rest_shape(nodes: &[serde_json::Value]) -> Vec<serde_json::Value> {
    nodes
        .iter()
        .map(|review| {
            serde_json::json!({
                "user": { "login": review["author"]["login"].as_str().unwrap_or_default() },
                "state": review["state"].as_str().unwrap_or_default(),
            })
        })
        .collect()
}

/// Map GraphQL's `MergeableState` onto the REST-shaped tri-state.
///
/// `mergeable_state` has no GraphQL counterpart without a preview-only field,
/// and nothing branches on it, so it is synthesized here the same way the
/// Forgejo backend synthesizes its own.
fn parse_graphql_mergeability(node: &serde_json::Value) -> Option<PrMergeability> {
    let (mergeable, state) = match node["mergeable"].as_str()? {
        "MERGEABLE" => (Some(true), "clean"),
        "CONFLICTING" => (Some(false), "dirty"),
        // UNKNOWN means GitHub has not finished computing the merge commit yet.
        "UNKNOWN" => (None, "unknown"),
        _ => return None,
    };
    Some(PrMergeability {
        mergeable,
        mergeable_state: state.to_string(),
    })
}

/// Pull one PR's bundle out of a GraphQL node, noting anything truncated.
fn parse_graphql_pr(node: &serde_json::Value) -> (PrStatusBundle, Truncation) {
    let mut truncation = Truncation::default();

    let reviews = node["reviews"]["nodes"].as_array().map(|nodes| {
        if node["reviews"]["totalCount"].as_u64().unwrap_or(0) > GRAPHQL_PAGE_SIZE as u64 {
            truncation.reviews = true;
        }
        parse_review_summary(&reviews_to_rest_shape(nodes))
    });

    // A commit with no CI at all has a null rollup, which is not truncation —
    // it is a genuine "no checks", and parse_checks_status maps empty to None.
    let rollup = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
    let checks = if rollup.is_null() {
        Some(ChecksStatus::None)
    } else {
        rollup["contexts"]["nodes"].as_array().map(|contexts| {
            if rollup["contexts"]["totalCount"].as_u64().unwrap_or(0) > GRAPHQL_PAGE_SIZE as u64 {
                truncation.checks = true;
            }
            let (check_runs, status) = contexts_to_rest_shape(contexts);
            parse_checks_status(&check_runs, &status)
        })
    };

    (
        PrStatusBundle {
            mergeability: parse_graphql_mergeability(node),
            checks,
            reviews,
        },
        truncation,
    )
}

/// Build one aliased query covering every PR in the batch.
///
/// Owner and repo travel as variables so they cannot break out of the query;
/// PR numbers are `u64`, so interpolating them is safe by construction.
fn build_batch_query(numbers: &[u64]) -> String {
    let mut query = String::from("query($owner: String!, $repo: String!) {\n  repository(owner: $owner, name: $repo) {\n");
    for (i, number) in numbers.iter().enumerate() {
        query.push_str(&format!("    pr{i}: pullRequest(number: {number}) {{ ...PrStatus }}\n"));
    }
    query.push_str("  }\n}\n");
    query.push_str(PR_STATUS_FRAGMENT);
    query
}

/// Track each reviewer's latest meaningful review state.
/// COMMENTED and PENDING don't change approval status on GitHub,
/// so we skip them to avoid overwriting a valid APPROVED state.
fn parse_review_summary(reviews: &[serde_json::Value]) -> ReviewSummary {
    let mut latest: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for review in reviews {
        let user = review["user"]["login"].as_str().unwrap_or_default();
        let state = review["state"].as_str().unwrap_or_default();
        if !user.is_empty()
            && matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED")
        {
            latest.insert(user.to_string(), state.to_string());
        }
    }

    let approved_count = latest.values().filter(|s| *s == "APPROVED").count() as u32;
    let changes_requested = latest.values().any(|s| s == "CHANGES_REQUESTED");

    ReviewSummary {
        approved_count,
        changes_requested,
    }
}

impl Forge for GitHubForge {
    fn list_open_prs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<PullRequest>> {
        let path = format!("repos/{owner}/{repo}/pulls?state=open&per_page=100");
        let items = self.client.get_paginated(&path)?;
        serde_json::from_value(serde_json::Value::Array(items))
            .context("failed to parse PR list response")
    }

    fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
        draft: bool,
    ) -> Result<PullRequest> {
        let path = format!("repos/{owner}/{repo}/pulls");
        let mut json_body = serde_json::json!({
            "title": title,
            "head": head,
            "base": base,
            "body": body,
        });
        if draft {
            json_body["draft"] = serde_json::json!(true);
        }
        let output = self.client.post(&path, &json_body)?;
        serde_json::from_value(output).context("failed to parse created PR response")
    }

    fn update_pr_base(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        base: &str,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        self.client.patch(&path, &serde_json::json!({ "base": base }))?;
        Ok(())
    }

    fn request_reviewers(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        reviewers: &[String],
    ) -> Result<()> {
        if reviewers.is_empty() {
            return Ok(());
        }
        let path = format!("repos/{owner}/{repo}/pulls/{number}/requested_reviewers");
        self.client.post(&path, &serde_json::json!({ "reviewers": reviewers }))?;
        Ok(())
    }

    fn list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>> {
        let path = format!("repos/{owner}/{repo}/issues/{number}/comments?per_page=100");
        let items = self.client.get_paginated(&path)?;
        serde_json::from_value(serde_json::Value::Array(items))
            .context("failed to parse comments response")
    }

    fn create_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let path = format!("repos/{owner}/{repo}/issues/{number}/comments");
        let output = self.client.post(&path, &serde_json::json!({ "body": body }))?;
        serde_json::from_value(output).context("failed to parse created comment response")
    }

    fn update_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/issues/comments/{comment_id}");
        self.client.patch(&path, &serde_json::json!({ "body": body }))?;
        Ok(())
    }

    fn update_pr_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        self.client.patch(&path, &serde_json::json!({ "body": body }))?;
        Ok(())
    }

    fn mark_pr_ready(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<()> {
        // GitHub requires GraphQL for marking a PR as ready.
        // First fetch the node_id from REST, then use it in the mutation.
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        let pr = self.client.get(&path)?;
        let node_id = pr["node_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("PR response missing node_id field"))?;

        let query = "mutation($id: ID!) { markPullRequestReadyForReview(input: { pullRequestId: $id }) { clientMutationId } }";
        self.client.graphql(
            "graphql",
            query,
            &serde_json::json!({ "id": node_id }),
        )?;
        Ok(())
    }

    fn find_merged_pr(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let encoded_head = super::http::url_encode(head);
        let path = format!(
            "repos/{owner}/{repo}/pulls?head={owner}:{encoded_head}&state=closed"
        );
        let output = self.client.get(&path)?;
        let prs: Vec<PullRequest> = serde_json::from_value(output)
            .context("failed to parse closed PR list response")?;
        Ok(prs.into_iter().find(|pr| pr.merged_at.is_some()))
    }

    fn get_authenticated_user(&self) -> Result<String> {
        let output = self.client.get("user")?;
        output["login"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("user response missing login field"))
    }

    fn get_authenticated_emails(&self) -> Result<Vec<String>> {
        Ok(crate::forge::parse_verified_emails(&self.client.get("user/emails")?))
    }

    fn native_stacks(&self, owner: &str, repo: &str) -> Result<Option<Vec<Stack>>> {
        let path = format!("repos/{owner}/{repo}/stacks");
        match self.client.get(&path) {
            Ok(value) => {
                let stacks = serde_json::from_value(value)
                    .context("failed to parse native stacks response")?;
                Ok(Some(stacks))
            }
            // A 404 is the documented "preview not enabled for this repo"
            // signal, which we treat as "no native stacks" rather than an error.
            Err(e) => match e.downcast_ref::<HttpError>() {
                Some(http) if http.status == 404 => Ok(None),
                _ => Err(e),
            },
        }
    }

    fn base_dismisses_stale_approvals(
        &self,
        owner: &str,
        repo: &str,
        base_branch: &str,
    ) -> Result<Option<bool>> {
        // Primary: classic branch protection via GraphQL (one cheap field).
        if let Ok(Some(v)) = self.classic_protection_dismisses_stale(owner, repo, base_branch) {
            return Ok(Some(v));
        }
        // Fallback (also covers the GraphQL-error and no-classic-rule cases):
        // effective branch rules, which include rulesets. Errors map to None.
        Ok(self.rules_dismiss_stale(owner, repo, base_branch).unwrap_or(None))
    }

    fn merge_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        method: MergeMethod,
    ) -> Result<()> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}/merge");
        self.client.put(&path, &serde_json::json!({ "merge_method": method.to_string() }))?;
        Ok(())
    }

    fn batch_pr_status(
        &self,
        owner: &str,
        repo: &str,
        prs: &[(u64, String)],
    ) -> Option<HashMap<u64, PrStatusBundle>> {
        if prs.is_empty() {
            return None;
        }

        let chunks: Vec<Vec<(u64, String)>> = prs
            .chunks(GRAPHQL_BATCH_SIZE)
            .map(<[(u64, String)]>::to_vec)
            .collect();

        // Chunks are independent queries, so overlap them; a stack big enough to
        // need several is exactly the case that hurts most serially.
        let results = crate::parallel::map_bounded(
            &chunks,
            crate::parallel::MAX_CONCURRENT_REQUESTS,
            |chunk| {
                let numbers: Vec<u64> = chunk.iter().map(|(n, _)| *n).collect();
                let query = build_batch_query(&numbers);
                let data = self.client.graphql(
                    "graphql",
                    &query,
                    &serde_json::json!({ "owner": owner, "repo": repo }),
                )?;

                let mut bundles = Vec::new();
                for (i, (number, head)) in chunk.iter().enumerate() {
                    let node = &data["repository"][format!("pr{i}")];
                    if node.is_null() {
                        anyhow::bail!("GraphQL response missing pr{i} (PR #{number})");
                    }
                    let (bundle, truncation) = parse_graphql_pr(node);
                    bundles.push((*number, head.clone(), bundle, truncation));
                }
                Ok::<_, anyhow::Error>(bundles)
            },
        );

        // Any failure — a rejected token, SAML, a rate limit, an undocumented
        // error type, a query bug — drops the whole stack to the per-PR REST
        // path. REST is the reference implementation and produces the better
        // diagnostics, so the cost of being wrong here is latency, not accuracy.
        let mut collected = Vec::new();
        for result in results {
            match result {
                Ok(bundles) => collected.extend(bundles),
                Err(_) => return None,
            }
        }

        // GraphQL connections stop at 100. Refill the few PRs that overflowed
        // from REST, in parallel, rather than paginating inside the batch.
        let needs_refill: Vec<(u64, String, Truncation)> = collected
            .iter()
            .filter(|(_, _, _, t)| t.reviews || t.checks)
            .map(|(n, head, _, t)| {
                (
                    *n,
                    head.clone(),
                    Truncation {
                        reviews: t.reviews,
                        checks: t.checks,
                    },
                )
            })
            .collect();

        let refilled: HashMap<u64, (Option<ReviewSummary>, Option<ChecksStatus>)> = if needs_refill
            .is_empty()
        {
            HashMap::new()
        } else {
            crate::parallel::map_bounded(
                &needs_refill,
                crate::parallel::MAX_CONCURRENT_REQUESTS,
                |(number, head, truncation)| {
                    let reviews = truncation
                        .reviews
                        .then(|| self.get_pr_reviews(owner, repo, *number).ok())
                        .flatten();
                    let checks = truncation
                        .checks
                        .then(|| self.get_pr_checks_status(owner, repo, head).ok())
                        .flatten();
                    (*number, (reviews, checks))
                },
            )
            .into_iter()
            .collect()
        };

        let mut map = HashMap::new();
        for (number, _, mut bundle, _) in collected {
            if let Some((reviews, checks)) = refilled.get(&number) {
                // Keep the batched value when the REST refill itself failed:
                // a truncated summary still beats no summary.
                if let Some(reviews) = reviews {
                    bundle.reviews = Some(reviews.clone());
                }
                if let Some(checks) = checks {
                    bundle.checks = Some(checks.clone());
                }
            }
            map.insert(number, bundle);
        }
        Some(map)
    }

    fn get_pr_checks_status(
        &self,
        owner: &str,
        repo: &str,
        head_ref: &str,
    ) -> Result<ChecksStatus> {
        let encoded_ref = super::http::url_encode(head_ref);

        // Both endpoints wrap their array in an envelope and default to 30 per
        // page. Reading only the first page let a failing check outside it pass
        // as green — and `jjpr merge` gates on this when CI is required — so ask
        // for the largest page and follow the Link header for the rest.
        let check_runs_path =
            format!("repos/{owner}/{repo}/commits/{encoded_ref}/check-runs?per_page=100");
        let check_runs = self
            .client
            .get_paginated_envelope(&check_runs_path, "check_runs")?;

        let status_path =
            format!("repos/{owner}/{repo}/commits/{encoded_ref}/status?per_page=100");
        let statuses = self
            .client
            .get_paginated_envelope(&status_path, "statuses")?;

        Ok(parse_checks_status(
            &serde_json::json!({ "check_runs": check_runs }),
            &serde_json::json!({ "statuses": statuses }),
        ))
    }

    fn get_pr_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<ReviewSummary> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100");
        let items = self.client.get_paginated(&path)?;
        Ok(parse_review_summary(&items))
    }

    fn get_pr_state(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrState> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        let pr = self.client.get(&path)?;
        Ok(PrState {
            merged: pr["merged_at"].is_string(),
            state: pr["state"].as_str().unwrap_or("unknown").to_string(),
        })
    }

    fn get_pr_mergeability(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrMergeability> {
        let path = format!("repos/{owner}/{repo}/pulls/{number}");
        let pr = self.client.get(&path)?;

        let mergeable = pr["mergeable"].as_bool();
        let mergeable_state = pr["mergeable_state"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        Ok(PrMergeability {
            mergeable,
            mergeable_state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `GET /repos/{owner}/{repo}/stacks` list payload (captured live,
    /// SHAs truncated): one open 3-PR stack and one merged 2-PR stack.
    const STACKS_LIST_JSON: &str = r#"[
      {"id":12634,"number":16306,"node_id":"PRS_kwDOABcQ480xWg","url":"https://api.github.com/repos/o/r/stacks/16306","base":{"ref":"master"},"open":true,"created_at":"2026-07-21T17:07:25Z","pull_requests":[
        {"number":16298,"state":"open","draft":false,"merged_at":null,"head":{"ref":"feat/llm-confidence-scoring-module","sha":"c00dded0"}},
        {"number":16300,"state":"open","draft":false,"merged_at":null,"head":{"ref":"feat/work-type-classifier-confidence-rubric","sha":"afc6a11b"}},
        {"number":16301,"state":"open","draft":false,"merged_at":null,"head":{"ref":"feat/rap-exclusions-confidence-rubric","sha":"68dc10e1"}}
      ]},
      {"id":12612,"number":16305,"node_id":"PRS_kwDOABcQ480xRA","url":"https://api.github.com/repos/o/r/stacks/16305","base":{"ref":"master"},"open":false,"created_at":"2026-07-21T17:03:49Z","pull_requests":[
        {"number":16303,"state":"closed","draft":false,"merged_at":"2026-07-21T18:23:59Z","head":{"ref":"rap-exclusion-rule-objects","sha":"230f7568"}},
        {"number":16304,"state":"closed","draft":false,"merged_at":"2026-07-21T18:52:01Z","head":{"ref":"rap-work-location-proximity","sha":"ab6ca106"}}
      ]}
    ]"#;

    #[test]
    fn native_stacks_payload_deserializes_to_ordered_stacks() {
        let stacks: Vec<Stack> = serde_json::from_str(STACKS_LIST_JSON).unwrap();
        assert_eq!(stacks.len(), 2);

        let open = &stacks[0];
        assert_eq!(open.number, 16306);
        assert!(open.open);
        assert_eq!(open.base.ref_name, "master");
        assert!(open.node_id.starts_with("PRS_"));
        // pull_requests are bottom-to-top; every PR here is still open/unmerged.
        let numbers: Vec<u64> = open.pull_requests.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![16298, 16300, 16301]);
        assert!(open.pull_requests.iter().all(|p| p.merged_at.is_none()));
        assert_eq!(open.pull_requests[0].head.ref_name, "feat/llm-confidence-scoring-module");

        let merged = &stacks[1];
        assert!(!merged.open);
        assert!(merged.pull_requests.iter().all(|p| p.state == "closed" && p.merged_at.is_some()));
    }

    #[test]
    fn classic_dismisses_reads_branch_protection_rule() {
        // Protected + dismiss on.
        let on = serde_json::json!({"repository":{"ref":{"branchProtectionRule":{"dismissesStaleReviews":true}}}});
        assert_eq!(parse_classic_dismisses(&on), Some(true));
        // Protected + dismiss off.
        let off = serde_json::json!({"repository":{"ref":{"branchProtectionRule":{"dismissesStaleReviews":false}}}});
        assert_eq!(parse_classic_dismisses(&off), Some(false));
        // No classic rule (null under rulesets / unprotected) → None ⇒ check rulesets.
        let none = serde_json::json!({"repository":{"ref":{"branchProtectionRule":null}}});
        assert_eq!(parse_classic_dismisses(&none), None);
        // Missing branch (ref null) → None.
        let missing = serde_json::json!({"repository":{"ref":null}});
        assert_eq!(parse_classic_dismisses(&missing), None);
    }

    #[test]
    fn rules_dismiss_reads_pull_request_rule_param() {
        let with = serde_json::json!([
            {"type":"required_status_checks","parameters":{}},
            {"type":"pull_request","parameters":{"dismiss_stale_reviews_on_push":true,"required_approving_review_count":1}}
        ]);
        assert!(rules_dismiss_stale_on_push(&with));
        let without = serde_json::json!([
            {"type":"pull_request","parameters":{"dismiss_stale_reviews_on_push":false}}
        ]);
        assert!(!rules_dismiss_stale_on_push(&without));
        // No rules at all → false.
        assert!(!rules_dismiss_stale_on_push(&serde_json::json!([])));
    }

    /// Build a GraphQL CheckRun context node.
    fn check_run(conclusion: Option<&str>, status: &str) -> serde_json::Value {
        serde_json::json!({
            "__typename": "CheckRun",
            "conclusion": conclusion,
            "status": status,
        })
    }

    /// Build a GraphQL StatusContext node.
    fn status_context(state: &str) -> serde_json::Value {
        serde_json::json!({ "__typename": "StatusContext", "state": state })
    }

    fn graphql_pr(reviews: serde_json::Value, rollup: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "number": 1,
            "mergeable": "MERGEABLE",
            "reviews": reviews,
            "commits": { "nodes": [ { "commit": { "statusCheckRollup": rollup } } ] },
        })
    }

    fn rollup(nodes: Vec<serde_json::Value>, total: u64) -> serde_json::Value {
        serde_json::json!({ "contexts": { "totalCount": total, "nodes": nodes } })
    }

    // The batched and per-PR paths must agree exactly. These pin the GraphQL
    // enum spellings onto the REST vocabulary that parse_checks_status expects;
    // a drift here would show a green stack that REST calls red.
    #[test]
    fn graphql_check_enums_lower_into_rest_vocabulary() {
        let (check_runs, statuses) = contexts_to_rest_shape(&[
            check_run(Some("SUCCESS"), "COMPLETED"),
            check_run(None, "IN_PROGRESS"),
            check_run(None, "QUEUED"),
            status_context("PENDING"),
        ]);
        assert_eq!(
            check_runs["check_runs"],
            serde_json::json!([
                {"conclusion": "success", "status": "completed"},
                {"conclusion": null, "status": "in_progress"},
                {"conclusion": null, "status": "queued"},
            ])
        );
        assert_eq!(statuses["statuses"], serde_json::json!([{"state": "pending"}]));
    }

    #[test]
    fn graphql_and_rest_agree_on_every_checks_outcome() {
        // (GraphQL contexts, equivalent REST payloads, expected)
        let cases: Vec<(Vec<serde_json::Value>, serde_json::Value, serde_json::Value, ChecksStatus)> = vec![
            (
                vec![check_run(Some("SUCCESS"), "COMPLETED"), check_run(Some("SKIPPED"), "COMPLETED"), check_run(Some("NEUTRAL"), "COMPLETED")],
                serde_json::json!({"check_runs": [{"conclusion": "success", "status": "completed"}, {"conclusion": "skipped", "status": "completed"}, {"conclusion": "neutral", "status": "completed"}]}),
                serde_json::json!({"statuses": []}),
                ChecksStatus::Pass,
            ),
            (
                vec![check_run(Some("SUCCESS"), "COMPLETED"), check_run(None, "IN_PROGRESS")],
                serde_json::json!({"check_runs": [{"conclusion": "success", "status": "completed"}, {"conclusion": null, "status": "in_progress"}]}),
                serde_json::json!({"statuses": []}),
                ChecksStatus::Pending,
            ),
            (
                vec![check_run(Some("FAILURE"), "COMPLETED")],
                serde_json::json!({"check_runs": [{"conclusion": "failure", "status": "completed"}]}),
                serde_json::json!({"statuses": []}),
                ChecksStatus::Fail,
            ),
            // A failure outranks a pending run regardless of order.
            (
                vec![check_run(None, "QUEUED"), check_run(Some("TIMED_OUT"), "COMPLETED")],
                serde_json::json!({"check_runs": [{"conclusion": null, "status": "queued"}, {"conclusion": "timed_out", "status": "completed"}]}),
                serde_json::json!({"statuses": []}),
                ChecksStatus::Fail,
            ),
            // ACTION_REQUIRED is a failure to jjpr, not a pass.
            (
                vec![check_run(Some("ACTION_REQUIRED"), "COMPLETED")],
                serde_json::json!({"check_runs": [{"conclusion": "action_required", "status": "completed"}]}),
                serde_json::json!({"statuses": []}),
                ChecksStatus::Fail,
            ),
            (
                vec![status_context("SUCCESS"), status_context("ERROR")],
                serde_json::json!({"check_runs": []}),
                serde_json::json!({"statuses": [{"state": "success"}, {"state": "error"}]}),
                ChecksStatus::Fail,
            ),
            (
                vec![],
                serde_json::json!({"check_runs": []}),
                serde_json::json!({"statuses": []}),
                ChecksStatus::None,
            ),
        ];

        for (contexts, rest_runs, rest_statuses, expected) in cases {
            let (gql_runs, gql_statuses) = contexts_to_rest_shape(&contexts);
            let via_graphql = parse_checks_status(&gql_runs, &gql_statuses);
            let via_rest = parse_checks_status(&rest_runs, &rest_statuses);
            assert_eq!(via_graphql, expected, "graphql path wrong for {contexts:?}");
            assert_eq!(via_rest, expected, "rest path wrong for {contexts:?}");
            assert_eq!(via_graphql, via_rest, "paths disagree for {contexts:?}");
        }
    }

    #[test]
    fn graphql_reviews_reshape_to_rest_and_keep_last_state_per_user() {
        let nodes = vec![
            serde_json::json!({"state": "APPROVED", "author": {"login": "ana"}}),
            serde_json::json!({"state": "COMMENTED", "author": {"login": "ana"}}),
            serde_json::json!({"state": "CHANGES_REQUESTED", "author": {"login": "bo"}}),
        ];
        let summary = parse_review_summary(&reviews_to_rest_shape(&nodes));
        // COMMENTED must not clobber ana's approval.
        assert_eq!(summary.approved_count, 1);
        assert!(summary.changes_requested);
    }

    #[test]
    fn graphql_review_with_null_author_does_not_panic() {
        // A review from a deleted account has a null author.
        let nodes = vec![serde_json::json!({"state": "APPROVED", "author": null})];
        let summary = parse_review_summary(&reviews_to_rest_shape(&nodes));
        // An empty login is skipped by parse_review_summary, matching REST.
        assert_eq!(summary.approved_count, 0);
    }

    #[test]
    fn mergeable_enum_maps_to_the_rest_tristate() {
        for (input, expected) in [
            ("MERGEABLE", Some(true)),
            ("CONFLICTING", Some(false)),
            ("UNKNOWN", None),
        ] {
            let node = serde_json::json!({ "mergeable": input });
            let parsed = parse_graphql_mergeability(&node).expect("known enum parses");
            assert_eq!(parsed.mergeable, expected, "for {input}");
        }
        assert!(parse_graphql_mergeability(&serde_json::json!({"mergeable": "SOMETHING_NEW"})).is_none());
        assert!(parse_graphql_mergeability(&serde_json::json!({})).is_none());
    }

    #[test]
    fn null_rollup_means_no_checks_not_missing_data() {
        // A commit with no CI configured has a null rollup. REST reports the
        // same case as two empty arrays, which is ChecksStatus::None.
        let node = graphql_pr(
            serde_json::json!({"totalCount": 0, "nodes": []}),
            serde_json::Value::Null,
        );
        let (bundle, truncation) = parse_graphql_pr(&node);
        assert_eq!(bundle.checks, Some(ChecksStatus::None));
        assert_eq!(truncation, Truncation::default());
    }

    #[test]
    fn over_100_connections_are_flagged_for_rest_refill() {
        let node = graphql_pr(
            serde_json::json!({"totalCount": 150, "nodes": []}),
            rollup(vec![check_run(Some("SUCCESS"), "COMPLETED")], 136),
        );
        let (_, truncation) = parse_graphql_pr(&node);
        assert!(truncation.reviews, "150 reviews exceeds the 100-node page");
        assert!(truncation.checks, "136 contexts exceeds the 100-node page");
    }

    #[test]
    fn connections_at_exactly_the_page_size_are_not_truncated() {
        let node = graphql_pr(
            serde_json::json!({"totalCount": 100, "nodes": []}),
            rollup(vec![], 100),
        );
        let (_, truncation) = parse_graphql_pr(&node);
        assert_eq!(truncation, Truncation::default(), "100 fits in one page");
    }

    #[test]
    fn batch_query_aliases_each_pr_and_parameterizes_the_repo() {
        let query = build_batch_query(&[7, 42]);
        assert!(query.contains("pr0: pullRequest(number: 7)"));
        assert!(query.contains("pr1: pullRequest(number: 42)"));
        // Owner/repo must travel as variables, never interpolated.
        assert!(query.contains("query($owner: String!, $repo: String!)"));
        assert!(query.contains("fragment PrStatus on PullRequest"));
        // Page sizes must stay within GitHub's 1..=100 connection limit.
        assert!(query.contains("reviews(last: 100)"));
        assert!(query.contains("contexts(first: 100)"));
    }

    #[test]
    fn test_parse_checks_all_passing() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "success", "status": "completed"},
                {"conclusion": "skipped", "status": "completed"},
            ]
        });
        let status = serde_json::json!({
            "statuses": [
                {"state": "success"}
            ]
        });
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pass);
    }

    #[test]
    fn test_parse_checks_pending() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": null, "status": "in_progress"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pending);
    }

    #[test]
    fn test_parse_checks_failure() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "failure", "status": "completed"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Fail);
    }

    #[test]
    fn test_parse_checks_none() {
        let check_runs = serde_json::json!({"check_runs": []});
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::None);
    }

    #[test]
    fn test_parse_checks_mixed_failure_wins() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "success", "status": "completed"},
                {"conclusion": "failure", "status": "completed"},
            ]
        });
        let status = serde_json::json!({
            "statuses": [{"state": "pending"}]
        });
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Fail);
    }

    #[test]
    fn test_parse_checks_queued_is_pending() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": null, "status": "queued"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pending);
    }

    #[test]
    fn test_parse_checks_neutral_passes() {
        let check_runs = serde_json::json!({
            "check_runs": [
                {"conclusion": "neutral", "status": "completed"},
            ]
        });
        let status = serde_json::json!({"statuses": []});
        assert_eq!(parse_checks_status(&check_runs, &status), ChecksStatus::Pass);
    }

    #[test]
    fn test_review_latest_state_wins() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "CHANGES_REQUESTED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 0);
        assert!(summary.changes_requested);
    }

    #[test]
    fn test_review_commented_does_not_override() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "COMMENTED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 1);
        assert!(!summary.changes_requested);
    }

    #[test]
    fn test_review_pending_does_not_override() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "PENDING"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 1);
        assert!(!summary.changes_requested);
    }

    #[test]
    fn test_review_multiple_reviewers() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "bob"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "charlie"}, "state": "CHANGES_REQUESTED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 2);
        assert!(summary.changes_requested);
    }

    #[test]
    fn test_review_dismissed_clears_approval() {
        let reviews = vec![
            serde_json::json!({"user": {"login": "alice"}, "state": "APPROVED"}),
            serde_json::json!({"user": {"login": "alice"}, "state": "DISMISSED"}),
        ];
        let summary = parse_review_summary(&reviews);
        assert_eq!(summary.approved_count, 0);
        assert!(!summary.changes_requested);
    }

    #[test]
    fn test_parse_mergeability_clean() {
        let pr = serde_json::json!({"mergeable": true, "mergeable_state": "clean"});
        assert_eq!(pr["mergeable"].as_bool(), Some(true));
        assert_eq!(pr["mergeable_state"].as_str(), Some("clean"));
    }

    #[test]
    fn test_parse_mergeability_dirty() {
        let pr = serde_json::json!({"mergeable": false, "mergeable_state": "dirty"});
        assert_eq!(pr["mergeable"].as_bool(), Some(false));
        assert_eq!(pr["mergeable_state"].as_str(), Some("dirty"));
    }

    #[test]
    fn test_parse_pr_basic_fields() {
        let json = r#"{
            "number": 42,
            "html_url": "https://github.com/owner/repo/pull/42",
            "title": "Add auth",
            "body": "Auth implementation",
            "base": {"ref": "main", "label": "owner:main"},
            "head": {"ref": "auth", "label": "owner:auth"},
            "draft": false,
            "node_id": "PR_kwDOABC123",
            "merged_at": null
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.html_url, "https://github.com/owner/repo/pull/42");
        assert_eq!(pr.title, "Add auth");
        assert_eq!(pr.base.ref_name, "main");
        assert_eq!(pr.head.ref_name, "auth");
        assert!(!pr.draft);
        assert_eq!(pr.node_id, "PR_kwDOABC123");
        assert!(pr.merged_at.is_none());
    }

    #[test]
    fn test_parse_pr_draft() {
        let json = r#"{
            "number": 7,
            "html_url": "https://github.com/o/r/pull/7",
            "title": "WIP",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "feat", "label": ""},
            "draft": true,
            "node_id": "PR_kwDOXYZ"
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert!(pr.draft);
        assert!(pr.body.is_none());
    }

    #[test]
    fn test_parse_pr_merged() {
        let json = r#"{
            "number": 99,
            "html_url": "https://github.com/o/r/pull/99",
            "title": "Done",
            "body": "merged",
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "feat", "label": ""},
            "draft": false,
            "node_id": "",
            "merged_at": "2024-06-15T10:30:00Z"
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.merged_at.as_deref(), Some("2024-06-15T10:30:00Z"));
    }

    #[test]
    fn test_parse_pr_requested_reviewers() {
        let json = r#"{
            "number": 42,
            "html_url": "https://github.com/o/r/pull/42",
            "title": "Auth",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "auth", "label": ""},
            "draft": false,
            "node_id": "",
            "requested_reviewers": [
                {"login": "alice", "id": 1},
                {"login": "bob", "id": 2}
            ]
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.requested_reviewers, vec!["alice", "bob"]);
    }

    #[test]
    fn test_parse_pr_null_requested_reviewers() {
        let json = r#"{
            "number": 42,
            "html_url": "https://github.com/o/r/pull/42",
            "title": "Auth",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "auth", "label": ""},
            "draft": false,
            "node_id": "",
            "requested_reviewers": null
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert!(pr.requested_reviewers.is_empty());
    }

    #[test]
    fn test_parse_pr_no_requested_reviewers() {
        let json = r#"{
            "number": 42,
            "html_url": "https://github.com/o/r/pull/42",
            "title": "Auth",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "auth", "label": ""},
            "draft": false,
            "node_id": ""
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert!(pr.requested_reviewers.is_empty());
    }

    #[test]
    fn test_parse_pr_author_from_user_login() {
        let json = r#"{
            "number": 42,
            "html_url": "https://github.com/o/r/pull/42",
            "title": "Auth",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "auth", "label": ""},
            "draft": false,
            "node_id": "",
            "user": {"login": "octocat", "id": 583231}
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.author, "octocat");
    }

    #[test]
    fn test_parse_pr_author_missing_or_null() {
        // No `user` key at all.
        let json = r#"{
            "number": 42,
            "html_url": "https://github.com/o/r/pull/42",
            "title": "Auth",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "auth", "label": ""}
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.author, "");

        // `user` present but null (e.g. a deleted account).
        let json_null = r#"{
            "number": 42,
            "html_url": "https://github.com/o/r/pull/42",
            "title": "Auth",
            "body": null,
            "base": {"ref": "main", "label": ""},
            "head": {"ref": "auth", "label": ""},
            "user": null
        }"#;
        let pr: PullRequest = serde_json::from_str(json_null).unwrap();
        assert_eq!(pr.author, "");
    }
}
