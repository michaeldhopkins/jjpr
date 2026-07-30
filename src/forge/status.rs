//! Reading PR status without paying a round trip per field.
//!
//! Three commands need the forge's view of a stack: `status` renders it, `merge`
//! gates on it, and `watch` polls it. Asked naively that is four requests per PR,
//! serially, in each of them.
//!
//! What they share is [`Forge::batch_pr_status`] and [`crate::parallel`]; what
//! they do not share is how much they need. `status` wants every field for every
//! PR. `merge` stops at the first blocked segment and must not pay for the
//! segments it never reaches. `watch` only ever looks at CI, and on GitLab a
//! review lookup alone costs three requests it would throw away. So the batch is
//! offered here as a prefetch the callers opt into, rather than as one
//! fetch-everything helper that would over-fetch for two of the three.

use std::collections::HashMap;

use super::types::{PrStatusBundle, PullRequest, RepoInfo};
use super::Forge;

/// Ask the forge for as much of `prs` as it can answer in one go.
///
/// Returns an empty map when the forge has no batch path or its batch call
/// failed. That is not an error: every caller can still answer per PR, and this
/// is only ever an optimization over doing so.
pub fn prefetch(
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    prs: &[&PullRequest],
) -> HashMap<u64, PrStatusBundle> {
    if prs.is_empty() {
        return HashMap::new();
    }
    let input: Vec<(u64, String)> = prs
        .iter()
        .map(|pr| (pr.number, pr.checks_ref().to_string()))
        .collect();
    forge
        .batch_pr_status(&repo_info.owner, &repo_info.repo, &input)
        .unwrap_or_default()
}

/// Every field the status view needs, for every PR, keyed by PR number.
///
/// Prefers the batch path and fans the remainder out concurrently. Used by
/// `status`, which renders all three fields and has no early exit to protect.
pub fn fetch_all(
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    prs: &[&PullRequest],
) -> HashMap<u64, PrStatusBundle> {
    if prs.is_empty() {
        return HashMap::new();
    }

    let mut bundles = prefetch(forge, repo_info, prs);

    // A GitHub token that cannot use GraphQL (SAML, a scope gap, a spent GraphQL
    // budget while REST still has one) lands here, as do GitLab and Forgejo.
    let missing: Vec<&PullRequest> = prs
        .iter()
        .filter(|pr| !bundles.contains_key(&pr.number))
        .copied()
        .collect();
    let fetched = crate::parallel::map_bounded(
        &missing,
        crate::parallel::MAX_CONCURRENT_REQUESTS,
        |pr| fetch_one(forge, repo_info, pr),
    );
    for (pr, bundle) in missing.iter().zip(fetched) {
        bundles.insert(pr.number, bundle);
    }
    bundles
}

/// Every field for one PR, over the per-PR endpoints.
///
/// An error on any field becomes `None`, which every caller already treats as
/// "unknown": `status` omits it, `merge` blocks on it.
pub fn fetch_one(
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    pr: &PullRequest,
) -> PrStatusBundle {
    let mergeability = forge
        .get_pr_mergeability(&repo_info.owner, &repo_info.repo, pr.number)
        .ok();
    let checks = forge
        .get_pr_checks_status(&repo_info.owner, &repo_info.repo, pr.checks_ref())
        .ok();
    let reviews = forge
        .get_pr_reviews(&repo_info.owner, &repo_info.repo, pr.number)
        .ok();
    PrStatusBundle {
        mergeability,
        checks,
        reviews,
    }
}

/// CI status for each PR, keyed by PR number, taking the batch answer where it
/// exists and fetching only CI for the rest.
///
/// Deliberately narrower than [`fetch_all`]: `watch` polls this every 30 seconds
/// and reads nothing but CI, so widening it to a full bundle would multiply a
/// recurring cost for fields that get dropped. A PR whose CI could not be read
/// at all is absent from the map rather than guessed at.
pub fn fetch_checks(
    forge: &dyn Forge,
    repo_info: &RepoInfo,
    prs: &[&PullRequest],
) -> HashMap<u64, super::types::ChecksStatus> {
    if prs.is_empty() {
        return HashMap::new();
    }

    let batched = prefetch(forge, repo_info, prs);
    let mut out: HashMap<u64, super::types::ChecksStatus> = batched
        .iter()
        .filter_map(|(number, bundle)| bundle.checks.clone().map(|c| (*number, c)))
        .collect();

    let missing: Vec<&PullRequest> = prs
        .iter()
        .filter(|pr| !out.contains_key(&pr.number))
        .copied()
        .collect();
    let fetched = crate::parallel::map_bounded(
        &missing,
        crate::parallel::MAX_CONCURRENT_REQUESTS,
        |pr| {
            forge
                .get_pr_checks_status(&repo_info.owner, &repo_info.repo, pr.checks_ref())
                .ok()
        },
    );
    for (pr, checks) in missing.iter().zip(fetched) {
        if let Some(checks) = checks {
            out.insert(pr.number, checks);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{
        ChecksStatus, IssueComment, MergeMethod, PrMergeability, PrState, PullRequestRef,
        ReviewSummary,
    };
    use anyhow::Result;
    use std::sync::Mutex;

    struct RecordingForge {
        batchable: Option<Vec<u64>>,
        mergeability_calls: Mutex<Vec<u64>>,
        checks_calls: Mutex<Vec<u64>>,
        reviews_calls: Mutex<Vec<u64>>,
    }

    impl RecordingForge {
        fn new(batchable: Option<Vec<u64>>) -> Self {
            Self {
                batchable,
                mergeability_calls: Mutex::new(Vec::new()),
                checks_calls: Mutex::new(Vec::new()),
                reviews_calls: Mutex::new(Vec::new()),
            }
        }
        fn sorted(calls: &Mutex<Vec<u64>>) -> Vec<u64> {
            let mut v = calls.lock().unwrap().clone();
            v.sort_unstable();
            v
        }
    }

    impl Forge for RecordingForge {
        fn batch_pr_status(
            &self,
            _o: &str,
            _r: &str,
            prs: &[(u64, String)],
        ) -> Option<HashMap<u64, PrStatusBundle>> {
            let answerable = self.batchable.as_ref()?;
            Some(
                prs.iter()
                    .filter(|(n, _)| answerable.contains(n))
                    .map(|(n, _)| {
                        (
                            *n,
                            PrStatusBundle {
                                mergeability: Some(PrMergeability {
                                    mergeable: Some(true),
                                    mergeable_state: "clean".to_string(),
                                }),
                                checks: Some(ChecksStatus::Pass),
                                reviews: Some(ReviewSummary {
                                    approved_count: 2,
                                    changes_requested: false,
                                }),
                            },
                        )
                    })
                    .collect(),
            )
        }
        fn get_pr_mergeability(&self, _o: &str, _r: &str, n: u64) -> Result<PrMergeability> {
            self.mergeability_calls.lock().unwrap().push(n);
            Ok(PrMergeability {
                mergeable: Some(false),
                mergeable_state: "dirty".to_string(),
            })
        }
        fn get_pr_checks_status(&self, _o: &str, _r: &str, head: &str) -> Result<ChecksStatus> {
            // The stub keys checks off the sha this helper is expected to pass.
            let number: u64 = head.trim_start_matches("sha").parse().unwrap_or(0);
            self.checks_calls.lock().unwrap().push(number);
            Ok(ChecksStatus::Fail)
        }
        fn get_pr_reviews(&self, _o: &str, _r: &str, n: u64) -> Result<ReviewSummary> {
            self.reviews_calls.lock().unwrap().push(n);
            Ok(ReviewSummary {
                approved_count: 0,
                changes_requested: true,
            })
        }
        fn list_open_prs(&self, _o: &str, _r: &str) -> Result<Vec<PullRequest>> {
            unimplemented!()
        }
        fn create_pr(&self, _o: &str, _r: &str, _t: &str, _b: &str, _h: &str, _ba: &str, _d: bool) -> Result<PullRequest> {
            unimplemented!()
        }
        fn update_pr_base(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn request_reviewers(&self, _o: &str, _r: &str, _n: u64, _v: &[String]) -> Result<()> {
            unimplemented!()
        }
        fn list_comments(&self, _o: &str, _r: &str, _n: u64) -> Result<Vec<IssueComment>> {
            unimplemented!()
        }
        fn create_comment(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<IssueComment> {
            unimplemented!()
        }
        fn update_comment(&self, _o: &str, _r: &str, _c: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn update_pr_body(&self, _o: &str, _r: &str, _n: u64, _b: &str) -> Result<()> {
            unimplemented!()
        }
        fn mark_pr_ready(&self, _o: &str, _r: &str, _n: u64) -> Result<()> {
            unimplemented!()
        }
        fn get_authenticated_user(&self) -> Result<String> {
            unimplemented!()
        }
        fn find_merged_pr(&self, _o: &str, _r: &str, _h: &str) -> Result<Option<PullRequest>> {
            unimplemented!()
        }
        fn merge_pr(&self, _o: &str, _r: &str, _n: u64, _m: MergeMethod) -> Result<()> {
            unimplemented!()
        }
        fn get_pr_state(&self, _o: &str, _r: &str, _n: u64) -> Result<PrState> {
            unimplemented!()
        }
    }

    fn pr(number: u64) -> PullRequest {
        PullRequest {
            number,
            html_url: String::new(),
            title: String::new(),
            body: None,
            base: PullRequestRef {
                ref_name: "main".to_string(),
                label: String::new(),
                sha: String::new(),
            },
            head: PullRequestRef {
                ref_name: format!("branch{number}"),
                label: String::new(),
                sha: format!("sha{number}"),
            },
            draft: false,
            node_id: String::new(),
            merged_at: None,
            requested_reviewers: vec![],
            author: "me".to_string(),
            stack: None,
        }
    }

    fn repo() -> RepoInfo {
        RepoInfo {
            owner: "o".to_string(),
            repo: "r".to_string(),
        }
    }

    #[test]
    fn fetch_all_uses_the_batch_and_skips_per_pr_reads() {
        let forge = RecordingForge::new(Some(vec![1, 2]));
        let prs = [pr(1), pr(2)];
        let refs: Vec<&PullRequest> = prs.iter().collect();
        let out = fetch_all(&forge, &repo(), &refs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[&1].checks, Some(ChecksStatus::Pass), "batched value");
        assert!(RecordingForge::sorted(&forge.mergeability_calls).is_empty());
        assert!(RecordingForge::sorted(&forge.reviews_calls).is_empty());
    }

    #[test]
    fn fetch_all_fills_only_what_the_batch_missed() {
        let forge = RecordingForge::new(Some(vec![1]));
        let prs = [pr(1), pr(2)];
        let refs: Vec<&PullRequest> = prs.iter().collect();
        let out = fetch_all(&forge, &repo(), &refs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[&1].checks, Some(ChecksStatus::Pass), "from batch");
        assert_eq!(out[&2].checks, Some(ChecksStatus::Fail), "from per-PR");
        assert_eq!(RecordingForge::sorted(&forge.mergeability_calls), vec![2]);
    }

    #[test]
    fn fetch_all_without_a_batch_path_covers_everything() {
        let forge = RecordingForge::new(None);
        let prs = [pr(1), pr(2), pr(3)];
        let refs: Vec<&PullRequest> = prs.iter().collect();
        let out = fetch_all(&forge, &repo(), &refs);
        assert_eq!(out.len(), 3);
        assert_eq!(RecordingForge::sorted(&forge.mergeability_calls), vec![1, 2, 3]);
        assert_eq!(RecordingForge::sorted(&forge.reviews_calls), vec![1, 2, 3]);
    }

    // watch polls this every 30s and reads nothing but CI, so it must never pull
    // the fields a full bundle would.
    #[test]
    fn fetch_checks_never_touches_reviews_or_mergeability() {
        let forge = RecordingForge::new(None);
        let prs = [pr(1), pr(2)];
        let refs: Vec<&PullRequest> = prs.iter().collect();
        let out = fetch_checks(&forge, &repo(), &refs);
        assert_eq!(out.len(), 2);
        assert_eq!(RecordingForge::sorted(&forge.checks_calls), vec![1, 2]);
        assert!(
            RecordingForge::sorted(&forge.reviews_calls).is_empty(),
            "a review lookup is three requests on GitLab and gets discarded here",
        );
        assert!(RecordingForge::sorted(&forge.mergeability_calls).is_empty());
    }

    #[test]
    fn fetch_checks_prefers_the_batch() {
        let forge = RecordingForge::new(Some(vec![1, 2]));
        let prs = [pr(1), pr(2)];
        let refs: Vec<&PullRequest> = prs.iter().collect();
        let out = fetch_checks(&forge, &repo(), &refs);
        assert_eq!(out[&1], ChecksStatus::Pass);
        assert!(RecordingForge::sorted(&forge.checks_calls).is_empty());
    }

    #[test]
    fn empty_input_makes_no_calls() {
        let forge = RecordingForge::new(Some(vec![1]));
        assert!(fetch_all(&forge, &repo(), &[]).is_empty());
        assert!(fetch_checks(&forge, &repo(), &[]).is_empty());
        assert!(prefetch(&forge, &repo(), &[]).is_empty());
        assert!(RecordingForge::sorted(&forge.checks_calls).is_empty());
    }
}
