//! Which jj is on `PATH`, for the few commands whose spelling depends on it.
//!
//! jjpr supports jj 0.36 and later. Everything it runs is spelled the same across
//! that range except where this module says otherwise; `tests/jj_compat.rs` and the
//! fixtures under `tests/fixtures/jj/` are the evidence.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct JjVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl JjVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

/// jj 0.38 made `jj git push --bookmark <name>` create and track a remote bookmark
/// that does not exist yet. 0.36 and 0.37 refuse ("Refusing to create new remote
/// bookmark") unless `git.push-new-bookmarks` is set, and have no `--allow-new`.
const PUSH_CREATES_NEW_SINCE: JjVersion = JjVersion::new(0, 38, 0);

/// Parse `jj --version` output: `jj 0.45.1`, or `jj 0.36.0-<hash>` from a release
/// build. The first whitespace-separated token starting with a digit is the version;
/// anything after its third number is ignored.
pub fn parse_jj_version(out: &str) -> Option<JjVersion> {
    let token = out
        .split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut numbers = token.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    Some(JjVersion::new(
        numbers.next()??,
        numbers.next()??,
        numbers.next()??,
    ))
}

/// The version of the `jj` on `PATH`, read once per process. `None` when jj is
/// missing or prints something unrecognisable.
pub fn installed_jj_version() -> Option<JjVersion> {
    static VERSION: OnceLock<Option<JjVersion>> = OnceLock::new();
    *VERSION.get_or_init(|| vcs_runner::jj_version().and_then(|v| parse_jj_version(&v)))
}

/// Global arguments that let `jj git push --bookmark <name>` publish a bookmark
/// the remote does not have yet. Empty from jj 0.38, which does that by default.
/// An unknown version gets the setting: every release from 0.33 to 0.45 accepts
/// it (0.36 to 0.40 print a deprecation warning, which jjpr does not show).
pub fn push_new_bookmark_args(version: Option<JjVersion>) -> &'static [&'static str] {
    if version.is_some_and(|v| v >= PUSH_CREATES_NEW_SINCE) {
        &[]
    } else {
        &["--config", "git.push-new-bookmarks=true"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_and_plain_version_strings() {
        assert_eq!(
            parse_jj_version("jj 0.45.1"),
            Some(JjVersion::new(0, 45, 1))
        );
        assert_eq!(
            parse_jj_version("jj 0.36.0-24f4e1083e8bcd6e5b8aaee3fa86e08cb7081d13"),
            Some(JjVersion::new(0, 36, 0))
        );
        assert_eq!(
            parse_jj_version("jj 1.2.3+dirty\n"),
            Some(JjVersion::new(1, 2, 3))
        );
    }

    #[test]
    fn rejects_what_is_not_a_version() {
        assert_eq!(parse_jj_version(""), None);
        assert_eq!(parse_jj_version("jj"), None);
        assert_eq!(parse_jj_version("jj 0.45"), None);
        assert_eq!(parse_jj_version("jj 0.x.1"), None);
    }

    #[test]
    fn push_needs_no_setting_from_0_38() {
        for v in [
            JjVersion::new(0, 38, 0),
            JjVersion::new(0, 45, 1),
            JjVersion::new(1, 0, 0),
        ] {
            assert!(push_new_bookmark_args(Some(v)).is_empty(), "{v:?}");
        }
    }

    #[test]
    fn push_sets_push_new_bookmarks_before_0_38_and_when_unknown() {
        for v in [
            Some(JjVersion::new(0, 37, 9)),
            Some(JjVersion::new(0, 36, 0)),
            None,
        ] {
            assert_eq!(
                push_new_bookmark_args(v),
                ["--config", "git.push-new-bookmarks=true"],
                "{v:?}"
            );
        }
    }

    /// The version jjpr acts on is the one `jj --version` on PATH reports. With
    /// `None` the push still works (the setting is harmless from 0.38), so only
    /// comparing against the real binary shows the lookup is actually done.
    #[test]
    fn installed_version_is_the_jj_on_path() {
        let Ok(out) = std::process::Command::new("jj").arg("--version").output() else {
            return;
        };
        let expected = parse_jj_version(&String::from_utf8_lossy(&out.stdout));
        assert!(expected.is_some(), "jj --version is parseable");
        assert_eq!(installed_jj_version(), expected);
    }
}
