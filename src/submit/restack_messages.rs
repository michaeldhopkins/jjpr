//! What a restack says: the note before it rebases, and the warnings when it
//! cannot, each with the command or the recovery page that gets the user going.

use super::restack::abandon_revset;

/// Said instead of rebasing when [`Decision::CannotTell`]. `merged` is sorted.
/// No single command is safe here, so it points at the recovery page.
pub fn merge_commit_warning(bookmark: &str, trunk: &str, merged: &[String]) -> String {
    let verb = if merged.len() == 1 { "was" } else { "were" };
    let names = merged.join("', '");
    let see = crate::docs::recovering("a-merged-pr-sits-under-a-merge-commit");
    format!(
        "  Warning: '{names}' below '{bookmark}' {verb} merged, but '{bookmark}' \
         starts with a merge commit. Rebase it onto {trunk} yourself.\n  See {see}"
    )
}

/// Said when the forge could not be asked what merged. Nothing is known to
/// have merged, so jjpr does not rebase. If something did, `--skip-emptied`
/// from the oldest commit above trunk drops exactly what already landed.
pub fn lookup_failed_warning(bookmark: &str, trunk: &str, oldest: &str, error: &str) -> String {
    format!(
        "  Warning: could not check whether a PR below '{bookmark}' was merged: {error}\n  \
         If one was, run this and submit again: jj rebase -s {oldest} -d {trunk} --skip-emptied"
    )
}

/// Said when the merged commits could not be abandoned after the survivor
/// moved. The survivor's PR is already right; this only tidies the log.
pub fn abandon_failed_warning(bookmark: &str, head: &str, error: &str) -> String {
    format!(
        "  Warning: could not abandon the merged commits below '{bookmark}': {error}\n  \
         '{bookmark}' is rebased already. To drop them, run: jj abandon '{}'",
        abandon_revset(head)
    )
}

/// `'top' onto main ('bottom' below it was merged)`: what the restack does and
/// why, after "Rebasing" or "Would rebase".
pub fn restack_note(bookmark: &str, trunk: &str, mut merged: Vec<String>) -> String {
    merged.sort();
    let verb = if merged.len() == 1 { "was" } else { "were" };
    let names = merged.join("', '");
    format!("'{bookmark}' onto {trunk} ('{names}' below it {verb} merged)")
}

/// The error when the restack's own rebase fails: the same rebase, spelled
/// with the trunk's name, for the user to run once jj's complaint is fixed.
pub fn rebase_failed(restack: &super::restack::Restack, trunk: &str) -> String {
    let mut command = format!("jj rebase -s {}", restack.root);
    for destination in &restack.onto {
        let destination = if destination == "trunk()" {
            trunk
        } else {
            destination
        };
        command.push_str(&format!(" -d {destination}"));
    }
    if restack.skip_emptied {
        command.push_str(" --skip-emptied");
    }
    format!(
        "failed to rebase '{}' onto {trunk}. To do it yourself: {command}",
        restack.bookmark
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submit::restack::Restack;

    #[test]
    fn rebase_failed_spells_out_the_same_rebase() {
        let mut restack = Restack {
            bookmark: "top".to_string(),
            root: "xkqv".to_string(),
            onto: vec!["trunk()".to_string(), "o1".to_string()],
            skip_emptied: false,
            abandon: None,
        };
        assert_eq!(
            rebase_failed(&restack, "main"),
            "failed to rebase 'top' onto main. To do it yourself: jj rebase -s xkqv -d main -d o1"
        );
        restack.onto.truncate(1);
        restack.skip_emptied = true;
        assert!(rebase_failed(&restack, "main").ends_with("-s xkqv -d main --skip-emptied"));
    }

    #[test]
    fn restack_note_names_what_merged_in_order_and_agrees_in_number() {
        assert_eq!(
            restack_note("top", "main", vec!["bottom".to_string()]),
            "'top' onto main ('bottom' below it was merged)"
        );
        assert_eq!(
            restack_note("top", "main", vec!["b".to_string(), "a".to_string()]),
            "'top' onto main ('a', 'b' below it were merged)"
        );
    }

    #[test]
    fn merge_commit_warning_names_what_merged_and_what_to_do() {
        assert_eq!(
            merge_commit_warning("top", "main", &["bottom".to_string()]),
            "  Warning: 'bottom' below 'top' was merged, but 'top' starts with a merge \
             commit. Rebase it onto main yourself.\n  \
             See https://michaeldhopkins.com/docs/jjpr/recovering.html\
             #a-merged-pr-sits-under-a-merge-commit"
        );
        let two = ["a".to_string(), "b".to_string()];
        assert!(merge_commit_warning("top", "main", &two).contains("'a', 'b' below 'top' were"));
    }

    #[test]
    fn lookup_failed_warning_gives_the_rebase_to_run_if_something_merged() {
        assert_eq!(
            lookup_failed_warning("top", "main", "xkqvwmop", "HTTP 502"),
            "  Warning: could not check whether a PR below 'top' was merged: HTTP 502\n  \
             If one was, run this and submit again: \
             jj rebase -s xkqvwmop -d main --skip-emptied"
        );
    }

    #[test]
    fn abandon_failed_warning_gives_the_abandon_to_run() {
        assert_eq!(
            abandon_failed_warning("top", "b2", "jj exited 1"),
            format!(
                "  Warning: could not abandon the merged commits below 'top': jj exited 1\n  \
                 'top' is rebased already. To drop them, run: jj abandon '{}'",
                abandon_revset("b2")
            )
        );
    }
}
