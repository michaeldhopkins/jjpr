//! `jj git fetch` for [`super::runner::JjRunner`]: which remotes, and which failures are fatal.

use std::path::Path;

use anyhow::Result;
use vcs_runner::{is_transient_error, run_jj_utf8_with_retry};

use super::types::GitRemote;
use crate::verbose;

/// Fetch every remote. With `required` (issue #11), that remote is fetched first and must
/// succeed; the others are still fetched, since trunk can live on one, but a failure on them is
/// a warning: a mirror the network cannot reach must not stop a submit. Unset, every remote is
/// fetched in one call and any failure is fatal.
pub(super) fn fetch_remotes(
    repo: &Path,
    required: Option<&str>,
    remotes: impl FnOnce() -> Result<Vec<GitRemote>>,
) -> Result<()> {
    let Some(required) = required else {
        return fetch(repo, &["--all-remotes"]);
    };
    fetch(repo, &["--remote", required])?;
    for remote in remotes()? {
        if remote.name != required
            && let Err(e) = fetch(repo, &["--remote", &remote.name])
        {
            eprintln!(
                "  Warning: could not fetch remote '{}'; continuing without it.\n    {e}",
                remote.name
            );
        }
    }
    Ok(())
}

fn fetch(repo: &Path, args: &[&str]) -> Result<()> {
    // Fetch is pure-read into the git backend, so retrying on a transient
    // error (".lock", or "stale", which can follow a partial commit) is
    // safe here, unlike the mutating ops, which use plain `run_jj`.
    let args = [&["--ignore-working-copy", "git", "fetch"], args].concat();
    verbose::jj(&args, || {
        run_jj_utf8_with_retry(repo, &args, is_transient_error)
    })?;
    Ok(())
}
