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

/// What the reviews leave to do, `None` when approvals are met and something
/// other than a review (CI, conflicts) still stands in the way of "ready".
pub fn next_step(status: &PrStatusBundle, required: u32) -> Option<String> {
    let reviews = status.reviews.as_ref()?;
    if reviews.changes_requested {
        return Some("address the requested changes, push, and ask for re-review".to_string());
    }
    let missing = required.saturating_sub(reviews.approved_count);
    if missing > 0 {
        let s = if missing == 1 { "" } else { "s" };
        return Some(format!("{} more review{s} required", count_word(missing)));
    }
    let blocked = status
        .mergeability
        .as_ref()
        .is_some_and(|m| m.mergeable == Some(false))
        || matches!(
            status.checks,
            Some(ChecksStatus::Fail | ChecksStatus::Pending)
        );
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

/// The status line for a segment, then its `next:` line when there is one.
/// Empty when the forge reported nothing.
pub fn status_lines(status: &PrStatusBundle, required: u32) -> Vec<String> {
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
        parts.push(approvals_part(r, required));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("    {}", parts.join("  "))];
    if let Some(next) = next_step(status, required) {
        lines.push(format!("    next: {next}"));
    }
    lines
}
