//! `jj git fetch` for [`super::runner::JjRunner`]: which remotes, and which failures are fatal.

use std::path::Path;

use anyhow::Result;
use vcs_runner::{is_transient_error, run_jj_utf8_with_retry};

use super::types::GitRemote;
use crate::verbose;

/// Whether a fetch may update the checkout. A fetch that rewrites a commit under `@` rebases
/// `@`; with the checkout ignored, jj 0.36 to 0.38 leave it stale, and the next command that
/// takes it into account gives `@` a divergent copy. Commands that snapshot on purpose (submit,
/// merge) update it; watch, which never touches the checkout, does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Checkout {
    Update,
    Ignore,
}

/// Fetch every remote. With `required` (issue #11), that remote is fetched first and must
/// succeed; the others are still fetched, since trunk can live on one, but a failure on them is
/// a warning: a mirror the network cannot reach must not stop a submit. Unset, every remote is
/// fetched in one call and any failure is fatal.
pub(super) fn fetch_remotes(
    repo: &Path,
    required: Option<&str>,
    checkout: Checkout,
    remotes: impl FnOnce() -> Result<Vec<GitRemote>>,
) -> Result<()> {
    let Some(required) = required else {
        return fetch(repo, checkout, &["--all-remotes"]);
    };
    fetch(repo, checkout, &["--remote", required])?;
    for remote in remotes()? {
        if remote.name != required
            && let Err(e) = fetch(repo, checkout, &["--remote", &remote.name])
        {
            eprintln!(
                "  Warning: could not fetch remote '{}'; continuing without it.\n    {e}",
                remote.name
            );
        }
    }
    Ok(())
}

/// The jj arguments for one fetch.
fn fetch_args<'a>(checkout: Checkout, args: &[&'a str]) -> Vec<&'a str> {
    let ignore: &[&str] = match checkout {
        Checkout::Update => &[],
        Checkout::Ignore => &["--ignore-working-copy"],
    };
    [ignore, &["git", "fetch"], args].concat()
}

fn fetch(repo: &Path, checkout: Checkout, args: &[&str]) -> Result<()> {
    // Fetch is pure-read into the git backend, so retrying on a transient
    // error (".lock", or "stale", which can follow a partial commit) is
    // safe here, unlike the mutating ops, which use plain `run_jj`.
    let args = fetch_args(checkout, args);
    verbose::jj(&args, || {
        run_jj_utf8_with_retry(repo, &args, is_transient_error)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_ignored_checkout_adds_the_flag() {
        assert_eq!(
            fetch_args(Checkout::Ignore, &["--all-remotes"]),
            ["--ignore-working-copy", "git", "fetch", "--all-remotes"]
        );
        assert_eq!(
            fetch_args(Checkout::Update, &["--remote", "origin"]),
            ["git", "fetch", "--remote", "origin"]
        );
    }
}
