#![no_main]

//! CREDENTIAL + AGREEMENT target for remote-URL parsing.
//!
//! Two properties, on top of never-panicking.
//!
//! 1. A CREDENTIAL NEVER REACHES THE REPO IDENTITY. jjpr learns which repo it is
//!    talking to by parsing a git remote, and a remote can carry an embedded
//!    credential (`https://user:ghp_…@github.com/owner/repo.git`) — a shape jjpr
//!    only learned to handle in 0d61ebb. The userinfo must be stripped, not
//!    absorbed: if a token ever lands in `owner`, `repo` or the host, jjpr builds
//!    API paths out of it and prints it in status output and PR comments, which
//!    leaks the token into places it is never coming back from. Fuzzing the *token*
//!    rather than the whole URL is what makes this assertable — the URL shape is
//!    fixed and known-good, so a failure means the stripping broke, not the parse.
//!
//! 2. DETECTION AND CONFIGURATION AGREE. `detect_forge` sniffs the host;
//!    `parse_url_as` is what runs when config pins `forge = "..."` explicitly. They
//!    are separate code paths over the same URL, so if they ever disagree about
//!    owner/repo, then setting the forge in config silently retargets jjpr at a
//!    different repository than the one it auto-detected.

use libfuzzer_sys::fuzz_target;

use jjpr::forge::remote::{detect_forge, extract_host, parse_url_as};

/// Hosts each of the three backends is expected to claim.
const HOSTS: &[&str] = &["github.com", "gitlab.com", "codeberg.org"];

/// A token safe to splice into the userinfo of a URL without changing its shape.
/// Anything that would move the host/path boundaries is rejected, so a failure
/// means the credential-stripping rule broke rather than the URL being malformed.
fn is_shape_preserving(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 200
        && !token.contains('@')
        && !token.contains('/')
        && !token.chars().any(|c| c.is_whitespace() || c.is_control())
}

fuzz_target!(|data: &[u8]| {
    // Never-panic pass: the raw bytes as a URL, which is what an unrecognised or
    // hand-edited remote looks like.
    let raw = String::from_utf8_lossy(data);
    let _ = detect_forge(&raw);
    let _ = extract_host(&raw);

    let Ok(token) = std::str::from_utf8(data) else {
        return;
    };
    if !is_shape_preserving(token) {
        return;
    }

    for host in HOSTS {
        for url in [
            format!("https://user:{token}@{host}/owner/repo.git"),
            format!("https://{token}@{host}/owner/repo.git"),
        ] {
            // Detection must SUCCEED, not merely be leak-free if it happens to
            // fire. Skipping a `None` here made the target blind to the very
            // regression it exists for: with the userinfo strip removed, the host
            // reads as `user:tok@github.com`, `is_github_host` rejects it,
            // `detect_forge` returns None — and a `continue` would call that fine.
            // Mutation-tested: deleting the strip left this target green for 30s
            // until this became an assertion.
            //
            // Sound to require, because `is_shape_preserving` guarantees the token
            // holds no `@`, so the last `@` is always the userinfo separator and
            // the host always resolves to the known constant.
            let (kind, info) = detect_forge(&url).unwrap_or_else(|| {
                panic!("a credential-embedded remote for a known host was not recognised: {url}")
            });

            // Assert EXACT equality with the known-good constants rather than
            // "the token does not appear in the output". A substring check reads
            // like the stronger statement but is the weaker one, and it is unsound
            // in the direction that matters: a one-character token like "d" is a
            // substring of "codeberg.org", so it reports a leak that did not
            // happen. Pinning the exact expected value rules out every leak,
            // including a partial one, with no dependence on how distinctive the
            // fuzzer's token happened to be.
            assert_eq!(
                (info.owner.as_str(), info.repo.as_str()),
                ("owner", "repo"),
                "userinfo reached the repo identity for {url}"
            );
            assert_eq!(
                extract_host(&url),
                Some(*host),
                "userinfo reached the host for {url}"
            );

            // Property 2: the config-pinned path must agree with detection.
            let pinned = parse_url_as(&url, kind)
                .unwrap_or_else(|| panic!("detect_forge accepted {url} but parse_url_as did not"));
            assert_eq!(
                (pinned.owner, pinned.repo),
                (info.owner, info.repo),
                "detect_forge and parse_url_as disagree on {url}"
            );
        }
    }
});
