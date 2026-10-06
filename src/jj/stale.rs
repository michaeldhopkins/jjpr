//! Bookmarks jj cannot place: they point at a missing or conflicted commit,
//! usually after their PR was merged on the forge. jjpr skips them and warns
//! once per bookmark for the life of a runner, since `watch` lists bookmarks on
//! every poll and repeating the same warning buried the output that mattered.

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct StaleBookmarks(Mutex<HashSet<String>>);

impl StaleBookmarks {
    /// Warn about each name not warned about before, and return those names.
    /// The hint is `jj bookmark forget` on its own: it touches one local
    /// bookmark and pushes nothing, unlike `jj git push --deleted`, which
    /// pushes every pending deletion in the repo.
    pub fn warn(&self, names: Vec<String>) -> Vec<String> {
        let mut warned = self.0.lock().expect("poisoned");
        let mut fresh = Vec::new();
        for name in names {
            if !warned.insert(name.clone()) {
                continue;
            }
            eprintln!("{}", warning(&name));
            fresh.push(name);
        }
        fresh
    }

    /// Every name warned about so far, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.0.lock().expect("poisoned").iter().cloned().collect();
        names.sort();
        names
    }

    pub fn remove(&self, name: &str) {
        self.0.lock().expect("poisoned").remove(name);
    }
}

/// The warning for one stale bookmark.
pub fn warning(name: &str) -> String {
    format!(
        "  Warning: skipping '{name}', which points to a missing or conflicted commit \
         (usually after its PR was merged on the forge).\n    To remove it: jj bookmark forget {name}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_once_per_name_and_forgets_on_request() {
        let stale = StaleBookmarks::default();
        assert_eq!(stale.warn(vec!["b".into(), "a".into()]), vec!["b", "a"]);
        assert_eq!(stale.warn(vec!["a".into(), "c".into()]), vec!["c"]);
        assert_eq!(stale.names(), vec!["a", "b", "c"]);
        stale.remove("b");
        assert_eq!(stale.names(), vec!["a", "c"]);
        assert_eq!(
            warning("a"),
            "  Warning: skipping 'a', which points to a missing or conflicted commit (usually \
             after its PR was merged on the forge).\n    To remove it: jj bookmark forget a"
        );
    }
}
