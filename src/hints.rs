//! What jjpr tells the user when it stops: the exact command that gets them
//! going again, or, where no single command is safe, a link to the section of
//! the recovery page that lists the options. Kept here so the messages read
//! alike and so the files that print them do not grow.

use crate::docs::recovering;

/// The command that shows conflicted commits, and how to resolve the first.
fn resolve_conflicts(bookmark: &str, first: Option<&str>) -> String {
    match first {
        Some(change) => format!(
            "Resolve them with jj new {change}, fix the files, then jj squash. \
             jjpr pushes '{bookmark}' on its next run."
        ),
        None => format!(
            "Find them with jj log -r 'conflicts() & trunk()..{bookmark}'. \
             jjpr pushes '{bookmark}' on its next run."
        ),
    }
}

/// After merging the new base into a bookmark left conflicts.
pub fn merge_conflict(base: &str, bookmark: &str, first: Option<&str>) -> String {
    format!(
        "Merging '{base}' into '{bookmark}' left conflicts, so '{bookmark}' was not pushed. {}",
        resolve_conflicts(bookmark, first)
    )
}

/// After rebasing a bookmark onto the new base left conflicts.
pub fn rebase_conflict(bookmark: &str, base: &str, first: Option<&str>) -> String {
    format!(
        "Rebasing '{bookmark}' onto '{base}' left conflicts, so '{bookmark}' was not pushed. {}",
        resolve_conflicts(bookmark, first)
    )
}

/// The next change to restack is divergent, so jjpr stops before touching it.
pub fn divergent_change(change: &str, count: usize) -> String {
    format!(
        "Change '{change}' is divergent: {count} commits share it, so jjpr stopped. \
         List them with jj log -r 'change_id({change})', then drop the one you do not \
         want with jj abandon <commit>. See {}",
        recovering("submit-or-merge-stops-on-a-divergent-change")
    )
}

/// Appended when a concurrent jj process left divergent changes behind.
pub fn concurrent_divergence(changes: &[String]) -> String {
    let revset = changes
        .iter()
        .map(|c| format!("change_id({c})"))
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        " The stack has a divergent change ({}). List the copies with jj log -r '{revset}', \
         then drop the stale one with jj abandon <commit>.",
        changes.join(", ")
    )
}

/// Another jj command changed the repo mid-restack. `restored` says whether
/// jjpr undid its own restack or never started it; either way nothing is lost.
pub fn concurrent_pause(restored: bool, divergent: &[String]) -> String {
    let what = if restored {
        "jjpr undid its restack"
    } else {
        "jjpr left the stack alone"
    };
    let mut message = format!(
        "Paused: another jj command changed the repo while jjpr was restacking. {what}, \
         your work is intact, and it tries again on the next poll."
    );
    if !divergent.is_empty() {
        message.push_str(&concurrent_divergence(divergent));
    }
    message.push_str(" If another jj or jjpr process is running on this repo, stop it.");
    message
}

/// Submit's refusal to publish a divergent change, naming each change's
/// commits and the abandon that keeps either one.
pub fn divergent_refusal(changes: &[crate::submit::plan::DivergentChange]) -> String {
    let mut out = String::from(
        "Refusing to submit: a change in this stack is divergent. Nothing has been pushed.\n",
    );
    for d in changes {
        let short: String = d.change_id.chars().take(12).collect();
        let (count, commits) = (d.commit_ids.len(), d.commit_ids.join(", "));
        out.push_str(&format!(
            "\n  change {short} is on {count} commits: {commits}"
        ));
        if !d.bookmarks.is_empty() {
            out.push_str(&format!(" (bookmarks: {})", d.bookmarks.join(", ")));
        }
        let keep = d
            .commit_ids
            .iter()
            .map(|c| format!("jj abandon {c}"))
            .collect::<Vec<_>>()
            .join(", or ");
        out.push_str(&format!("\n    Keep one: {keep}"));
    }
    out.push_str(&format!(
        "\n\nTo keep both as separate changes instead: jj duplicate <commit>, then \
         jj abandon <commit>.\nSee {}",
        recovering("submit-or-merge-stops-on-a-divergent-change")
    ));
    out
}

/// Wraps a failed push of `bookmark`.
pub fn push_failed(bookmark: &str) -> String {
    format!(
        "could not push '{bookmark}'. See {}",
        recovering("a-push-failed")
    )
}

/// Watch stops after too many errors in a row.
pub fn watch_gave_up() -> String {
    format!(
        "  Too many errors in a row. Giving up.\n  See {}",
        recovering("watch-gave-up")
    )
}

/// The line under any error the forge's API returned.
pub fn forge_error() -> String {
    format!("See {}", recovering("the-forge-returned-an-error"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "https://michaeldhopkins.com/docs/jjpr/recovering.html";

    #[test]
    fn conflicts_name_the_first_conflicted_change_to_resolve() {
        assert_eq!(
            rebase_conflict("b", "main", Some("xkqv")),
            "Rebasing 'b' onto 'main' left conflicts, so 'b' was not pushed. Resolve them \
             with jj new xkqv, fix the files, then jj squash. jjpr pushes 'b' on its next run."
        );
        assert_eq!(
            merge_conflict("main", "b", None),
            "Merging 'main' into 'b' left conflicts, so 'b' was not pushed. Find them with \
             jj log -r 'conflicts() & trunk()..b'. jjpr pushes 'b' on its next run."
        );
    }

    #[test]
    fn divergence_says_how_to_keep_one_copy() {
        assert_eq!(
            divergent_change("xkqv", 2),
            format!(
                "Change 'xkqv' is divergent: 2 commits share it, so jjpr stopped. List them \
                 with jj log -r 'change_id(xkqv)', then drop the one you do not want with \
                 jj abandon <commit>. See {PAGE}#submit-or-merge-stops-on-a-divergent-change"
            )
        );
        assert_eq!(
            concurrent_divergence(&["a".to_string(), "b".to_string()]),
            " The stack has a divergent change (a, b). List the copies with \
             jj log -r 'change_id(a) | change_id(b)', then drop the stale one with \
             jj abandon <commit>."
        );
        let change = crate::submit::plan::DivergentChange {
            change_id: "xkqvwmopzzzzzz".to_string(),
            commit_ids: vec!["aa".to_string(), "bb".to_string()],
            bookmarks: vec!["top".to_string()],
        };
        assert_eq!(
            divergent_refusal(&[change]),
            format!(
                "Refusing to submit: a change in this stack is divergent. Nothing has been \
                 pushed.\n\n  change xkqvwmopzzzz is on 2 commits: aa, bb (bookmarks: top)\
                 \n    Keep one: jj abandon aa, or jj abandon bb\n\nTo keep both as separate \
                 changes instead: jj duplicate <commit>, then jj abandon <commit>.\nSee \
                 {PAGE}#submit-or-merge-stops-on-a-divergent-change"
            )
        );
        assert_eq!(
            concurrent_pause(true, &[]),
            "Paused: another jj command changed the repo while jjpr was restacking. jjpr \
             undid its restack, your work is intact, and it tries again on the next poll. \
             If another jj or jjpr process is running on this repo, stop it."
        );
        let left = concurrent_pause(false, &["a".to_string()]);
        assert!(left.contains("jjpr left the stack alone"), "{left}");
        assert!(left.contains("jj log -r 'change_id(a)'"), "{left}");
    }

    #[test]
    fn stops_link_their_section_of_the_recovery_page() {
        assert_eq!(
            push_failed("b"),
            format!("could not push 'b'. See {PAGE}#a-push-failed")
        );
        assert_eq!(
            watch_gave_up(),
            format!("  Too many errors in a row. Giving up.\n  See {PAGE}#watch-gave-up")
        );
        assert_eq!(
            forge_error(),
            format!("See {PAGE}#the-forge-returned-an-error")
        );
    }
}
