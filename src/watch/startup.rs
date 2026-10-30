//! Pre-loop status line for `jjpr watch`.

use crate::forge::Forge;
use crate::jj::Jj;

/// One-time pre-loop summary so the user knows what watch is working
/// with. Best-effort: silently skip if the forge or jj queries fail.
pub(super) fn print_initial_watch_status(
    jj: &dyn Jj,
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
    target_bookmark: &str,
) {
    let Ok(initial_prs) = forge.list_open_prs(owner, repo) else {
        return;
    };
    let pr_map = crate::forge::build_pr_map(initial_prs, owner);
    let segments = super::rediscover_segments(jj, target_bookmark).unwrap_or_default();
    let with_pr: Vec<_> = segments
        .iter()
        .filter(|s| pr_map.contains_key(&s.bookmark.name))
        .collect();
    let without_pr: Vec<_> = segments
        .iter()
        .filter(|s| !pr_map.contains_key(&s.bookmark.name))
        .collect();
    if with_pr.is_empty() && without_pr.is_empty() {
        return;
    }
    let plural = if segments.len() == 1 { "" } else { "s" };
    let with_pr_suffix = if !with_pr.is_empty() {
        format!(
            ", {} with existing PR{}",
            with_pr.len(),
            if with_pr.len() == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    println!(
        "  {} bookmark{plural} in stack{with_pr_suffix}",
        segments.len()
    );
    if !without_pr.is_empty() {
        let names: Vec<_> = without_pr
            .iter()
            .map(|s| s.bookmark.name.as_str())
            .collect();
        println!("  Will create draft PRs for: {}\n", names.join(", "));
    } else {
        println!();
    }
}
