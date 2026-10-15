//! The review half of `jjpr status`: approvals against the number required,
//! and the one thing to do next.

use crate::forge::{ChecksStatus, PrStatusBundle, ReviewSummary};

/// `one` to `nine` as words, digits above: a count inside a sentence.
pub fn count_word(n: u32) -> String {
    const WORDS: [&str; 9] = [
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ];
    match n {
        1..=9 => WORDS[n as usize - 1].to_string(),
        _ => n.to_string(),
    }
}

/// What `jjpr merge` requires before it lands a PR, as far as status can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub required_approvals: u32,
    pub require_ci_pass: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            required_approvals: 1,
            require_ci_pass: true,
        }
    }
}

impl Policy {
    pub fn of(config: &crate::config::Config) -> Self {
        Self {
            required_approvals: config.required_approvals,
            require_ci_pass: config.require_ci_pass,
        }
    }
}

/// What the reviews leave to do, `None` when approvals are met and something
/// other than a review (CI, conflicts) still stands in the way of "ready".
pub fn next_step(status: &PrStatusBundle, policy: Policy, merge_blocked: bool) -> Option<String> {
    let reviews = status.reviews.as_ref()?;
    if reviews.changes_requested {
        return Some("address the requested changes, push, and ask for re-review".to_string());
    }
    let missing = policy
        .required_approvals
        .saturating_sub(reviews.approved_count);
    if missing > 0 {
        let s = if missing == 1 { "" } else { "s" };
        return Some(format!("{} more review{s} required", count_word(missing)));
    }
    let blocked = merge_blocked
        || !status
            .mergeability
            .as_ref()
            .is_some_and(|m| m.mergeable == Some(true))
        || (policy.require_ci_pass && status.checks != Some(ChecksStatus::Pass));
    (!blocked).then(|| "ready to merge (jjpr merge, or jjpr watch will merge it)".to_string())
}

/// `✓ 1/1 approvals`, or the bare count when nothing is required.
pub fn approvals_part(reviews: &ReviewSummary, required: u32) -> String {
    let met = if required == 0 {
        reviews.approved_count > 0
    } else {
        reviews.approved_count >= required
    };
    let mark = if met { "\u{2713}" } else { "\u{2717}" };
    if required == 0 {
        let s = if reviews.approved_count == 1 { "" } else { "s" };
        return format!("{mark} {} approval{s}", reviews.approved_count);
    }
    format!("{mark} {}/{required} approvals", reviews.approved_count)
}

/// The mergeability half of a status line only: what matters for a coworker's base,
/// which you want to know can land, not its CI or reviews.
pub fn mergeability_line(status: &PrStatusBundle) -> String {
    match status.mergeability.as_ref().and_then(|m| m.mergeable) {
        Some(true) => "    \u{2713} mergeable".to_string(),
        Some(false) => "    \u{2717} conflicts".to_string(),
        None => String::new(),
    }
}

/// The status line for a segment, then its `next:` line when there is one.
/// `merge_blocked` is set when something other than the forge's own signals stops
/// `jjpr merge` (a native stack, a divergent change), so "ready" is not claimed.
/// Empty when the forge reported nothing.
pub fn status_lines(status: &PrStatusBundle, policy: Policy, merge_blocked: bool) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(m) = &status.mergeability {
        match m.mergeable {
            Some(true) => parts.push("\u{2713} mergeable".to_string()),
            Some(false) => parts.push("\u{2717} conflicts".to_string()),
            None => parts.push("? mergeability computing".to_string()),
        }
    }
    if let Some(checks) = &status.checks {
        match checks {
            ChecksStatus::Pass => parts.push("\u{2713} CI passing".to_string()),
            ChecksStatus::Fail => parts.push("\u{2717} CI failing".to_string()),
            ChecksStatus::Pending => parts.push("\u{2717} CI pending".to_string()),
            ChecksStatus::None => {}
        }
    }
    if let Some(r) = &status.reviews {
        if r.changes_requested {
            parts.push("\u{26a0} changes requested".to_string());
        }
        parts.push(approvals_part(r, policy.required_approvals));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("    {}", parts.join("  "))];
    if let Some(next) = next_step(status, policy, merge_blocked) {
        lines.push(format!("    next: {next}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::PrMergeability;
    use proptest::prelude::*;

    fn bundle(approved: u32, changes_requested: bool, checks: ChecksStatus) -> PrStatusBundle {
        PrStatusBundle {
            mergeability: Some(PrMergeability {
                mergeable: Some(true),
                mergeable_state: "clean".to_string(),
            }),
            checks: Some(checks),
            reviews: Some(ReviewSummary {
                approved_count: approved,
                changes_requested,
            }),
        }
    }

    fn pol(required: u32) -> Policy {
        Policy {
            required_approvals: required,
            require_ci_pass: true,
        }
    }

    const READY: &str = "    next: ready to merge (jjpr merge, or jjpr watch will merge it)";

    #[test]
    fn counts_are_words_up_to_nine_then_digits() {
        assert_eq!(count_word(1), "one");
        assert_eq!(count_word(9), "nine");
        assert_eq!(count_word(10), "10");
    }

    #[test]
    fn unapproved_pr_shows_the_count_against_the_requirement() {
        assert_eq!(
            status_lines(&bundle(0, false, ChecksStatus::Pass), pol(1), false),
            vec![
                "    \u{2713} mergeable  \u{2713} CI passing  \u{2717} 0/1 approvals",
                "    next: one more review required",
            ]
        );
        assert_eq!(
            status_lines(&bundle(1, false, ChecksStatus::Pass), pol(3), false)[1],
            "    next: two more reviews required"
        );
        assert_eq!(
            status_lines(&bundle(0, false, ChecksStatus::Pass), pol(10), false)[1],
            "    next: 10 more reviews required"
        );
    }

    #[test]
    fn changes_requested_outranks_the_count() {
        let lines = status_lines(&bundle(1, true, ChecksStatus::Pass), pol(1), false);
        assert_eq!(
            lines[0],
            "    \u{2713} mergeable  \u{2713} CI passing  \u{26a0} changes requested  \u{2713} 1/1 approvals"
        );
        assert_eq!(
            lines[1],
            "    next: address the requested changes, push, and ask for re-review"
        );
    }

    #[test]
    fn approved_pr_is_ready_to_merge() {
        assert_eq!(
            status_lines(&bundle(1, false, ChecksStatus::Pass), pol(1), false),
            vec![
                "    \u{2713} mergeable  \u{2713} CI passing  \u{2713} 1/1 approvals",
                READY
            ]
        );
    }

    #[test]
    fn ready_is_withheld_when_merge_would_refuse() {
        let lines = |s: &PrStatusBundle, blocked| status_lines(s, pol(1), blocked).len();
        assert_eq!(lines(&bundle(1, false, ChecksStatus::Fail), false), 1);
        assert_eq!(lines(&bundle(1, false, ChecksStatus::Pending), false), 1);
        assert_eq!(lines(&bundle(1, false, ChecksStatus::Pass), true), 1);
        for mergeable in [Some(false), None] {
            let mut s = bundle(1, false, ChecksStatus::Pass);
            s.mergeability = Some(PrMergeability {
                mergeable,
                mergeable_state: String::new(),
            });
            assert_eq!(lines(&s, false), 1, "{mergeable:?}");
        }
        let mut unknown = bundle(1, false, ChecksStatus::Pass);
        unknown.mergeability = None;
        assert_eq!(lines(&unknown, false), 1);
    }

    #[test]
    fn ci_only_gates_ready_when_the_config_requires_it() {
        let ready = |checks: ChecksStatus, require_ci_pass| {
            let policy = Policy {
                required_approvals: 1,
                require_ci_pass,
            };
            status_lines(&bundle(1, false, checks), policy, false).len() == 2
        };
        assert!(ready(ChecksStatus::Pass, true));
        assert!(!ready(ChecksStatus::Fail, true));
        assert!(!ready(ChecksStatus::None, true));
        assert!(ready(ChecksStatus::Fail, false));
        assert!(ready(ChecksStatus::None, false));
    }

    #[test]
    fn no_requirement_keeps_the_bare_count() {
        let lines = status_lines(&bundle(0, false, ChecksStatus::Pass), pol(0), false);
        assert!(lines[0].ends_with("\u{2717} 0 approvals"), "{lines:?}");
        assert_eq!(lines[1], READY);
        let one = status_lines(&bundle(1, false, ChecksStatus::Pass), pol(0), false);
        assert!(one[0].ends_with("\u{2713} 1 approval"), "{one:?}");
    }

    #[test]
    fn the_mergeability_line_is_all_a_coworkers_base_shows() {
        let mut s = bundle(0, false, ChecksStatus::Fail);
        assert_eq!(mergeability_line(&s), "    \u{2713} mergeable");
        s.mergeability = Some(PrMergeability {
            mergeable: Some(false),
            mergeable_state: String::new(),
        });
        assert_eq!(mergeability_line(&s), "    \u{2717} conflicts");
        s.mergeability = None;
        assert_eq!(mergeability_line(&s), "");
    }

    #[test]
    fn nothing_reported_prints_nothing() {
        assert!(status_lines(&PrStatusBundle::default(), pol(1), false).is_empty());
    }

    proptest! {
        // The missing count is required - approved, and the sentence agrees in number.
        #[test]
        fn next_step_counts_what_is_missing(approved in 0u32..40, required in 1u32..40) {
            let step = next_step(&bundle(approved, false, ChecksStatus::Pass), pol(required), false)
                .expect("a step");
            if approved >= required {
                prop_assert!(step.starts_with("ready to merge"));
            } else {
                let missing = required - approved;
                let expected = format!(
                    "{} more review{} required",
                    count_word(missing),
                    if missing == 1 { "" } else { "s" }
                );
                prop_assert_eq!(step, expected);
            }
        }

        // Words below ten, the digits from ten.
        #[test]
        fn count_word_is_a_word_below_ten_and_the_digits_above(n in 1u32..100_000) {
            let word = count_word(n);
            if n < 10 {
                prop_assert!(word.chars().all(|c| c.is_ascii_lowercase()));
            } else {
                prop_assert_eq!(word, n.to_string());
            }
        }
    }
}
