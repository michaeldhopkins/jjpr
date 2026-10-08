//! Forget a stale bookmark once its PR is known to be merged.
//!
//! A bookmark that points at a missing or conflicted commit is skipped with a
//! warning (see `JjRunner::get_my_bookmarks`). The usual cause is a PR merged
//! on the forge while the branch moved on both sides. When the forge confirms
//! the merge, the bookmark has nothing left to do, and `jj bookmark forget`
//! removes it locally without pushing a deletion. A bookmark whose PR is
//! open, closed unmerged or unknown is left for the user to judge.
//!
//! A merged PR is found by branch name, and a name can be reused: an old PR
//! from `fix` merged, a new one from `fix` still open. So a bookmark with an
//! open PR is never forgotten, and when the open PRs cannot be listed,
//! nothing is.

use crate::forge::{Forge, ForgeKind};
use crate::jj::Jj;

/// Forget each stale bookmark whose PR merged, saying so. Returns the names
/// forgotten. Lookup and forget failures leave the bookmark and its warning.
pub fn forget_merged(
    jj: &dyn Jj,
    forge: &dyn Forge,
    owner: &str,
    repo: &str,
    kind: ForgeKind,
) -> Vec<String> {
    let stale = jj.stale_bookmarks();
    if stale.is_empty() {
        return Vec::new();
    }
    let Ok(open) = forge.list_open_prs(owner, repo) else {
        return Vec::new();
    };
    let mut forgotten = Vec::new();
    for name in stale {
        if open.iter().any(|pr| pr.head.ref_name == name) {
            continue;
        }
        let Ok(Some(pr)) = forge.find_merged_pr(owner, repo, &name) else {
            continue;
        };
        if jj.forget_bookmark(&name).is_ok() {
            println!(
                "  Forgot the stale bookmark '{name}': {} was merged.",
                kind.format_ref(pr.number)
            );
            forgotten.push(name);
        }
    }
    forgotten
}
