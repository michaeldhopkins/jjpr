//! Waiting out a forge's rate limit instead of failing the command.
//!
//! Each forge says "slow down" its own way:
//!
//! - GitHub answers 429, or 403 for its primary and secondary limits, with
//!   `Retry-After` (seconds) or `x-ratelimit-remaining: 0` plus
//!   `x-ratelimit-reset` (epoch seconds).
//! - GitLab answers 429 with `Retry-After` and `RateLimit-Reset` (epoch).
//! - Forgejo and Gitea answer 429, with `Retry-After` where configured.
//!
//! A short wait is slept and the request sent again, up to [`MAX_RETRIES`]
//! times. A wait longer than [`MAX_WAIT`] (GitHub's hourly limit can reset
//! an hour out) is not slept: the error goes to the user, whose docs page
//! says to wait and run again. Every request jjpr sends is safe to resend
//! after a 429 or a rate-limit 403, since the forge did not act on it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use ureq::http::{HeaderMap, Response};

use super::http::HttpError;

/// Resends after a rate limit before giving up.
pub const MAX_RETRIES: u32 = 2;

/// The longest wait slept; a longer one is reported instead.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// GitHub's advice when a 429 names no time: wait a minute.
const DEFAULT_WAIT: Duration = Duration::from_secs(60);

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

fn seconds(headers: &HeaderMap, name: &str) -> Option<u64> {
    header(headers, name).and_then(|v| v.parse().ok())
}

/// How long to wait before resending, or `None` when the response is not a
/// rate limit. `now` is epoch seconds, so the reset headers can be read.
pub fn wait(status: u16, headers: &HeaderMap, now: u64) -> Option<Duration> {
    let remaining =
        header(headers, "x-ratelimit-remaining").or_else(|| header(headers, "ratelimit-remaining"));
    let exhausted = remaining == Some("0");
    let retry_after = seconds(headers, "retry-after");
    let limited = status == 429 || (status == 403 && (exhausted || retry_after.is_some()));
    if !limited {
        return None;
    }
    if let Some(secs) = retry_after {
        return Some(Duration::from_secs(secs));
    }
    // The reset time only matters once the quota is spent. A 429 with quota
    // left is GitHub's secondary limit, which asks for a minute.
    match reset_at(headers).filter(|_| remaining.is_none() || exhausted) {
        Some(at) => Some(Duration::from_secs(at.saturating_sub(now).max(1))),
        None => Some(DEFAULT_WAIT),
    }
}

/// When the forge's request quota refills, in epoch seconds: GitHub's
/// `x-ratelimit-reset` or GitLab's `RateLimit-Reset`.
pub fn reset_at(headers: &HeaderMap) -> Option<u64> {
    seconds(headers, "x-ratelimit-reset").or_else(|| seconds(headers, "ratelimit-reset"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Send with `attempt`, waiting out short rate limits, and turn a final
/// 4xx or 5xx into an [`HttpError`] carrying the body.
pub fn send(
    method: &str,
    path: &str,
    url: &str,
    mut attempt: impl FnMut() -> Result<Response<ureq::Body>, ureq::Error>,
) -> anyhow::Result<Response<ureq::Body>> {
    let mut retries = 0;
    let mut resp = loop {
        let resp = attempt().with_context(|| format!("{method} {url}"))?;
        match wait(resp.status().as_u16(), resp.headers(), now()) {
            Some(delay) if retries < MAX_RETRIES && delay <= MAX_WAIT => {
                eprintln!(
                    "  The forge is rate limiting jjpr. Trying again in {}s...",
                    delay.as_secs()
                );
                std::thread::sleep(delay);
                retries += 1;
            }
            _ => break resp,
        }
    };
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = resp
            .body_mut()
            .read_to_string()
            .unwrap_or_else(|_| String::from("<unreadable>"));
        let (method, path) = (method.to_string(), path.to_string());
        return Err(HttpError {
            status,
            method,
            path,
            body,
        }
        .into());
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::test_server::{StubServer, route, route_with_headers};
    use crate::forge::{AuthScheme, ForgeClient, PaginationStyle};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                ureq::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn wait_reads_each_forges_rate_limit_headers() {
        let now = 1_000;
        let secs = |s| Some(Duration::from_secs(s));
        assert_eq!(wait(429, &headers(&[("Retry-After", "7")]), now), secs(7));
        let github = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1030"),
        ]);
        assert_eq!(wait(403, &github, now), secs(30));
        let gitlab = headers(&[("RateLimit-Reset", "1012")]);
        assert_eq!(wait(429, &gitlab, now), secs(12));
        assert_eq!(wait(403, &headers(&[("Retry-After", "5")]), now), secs(5));
        assert_eq!(
            wait(429, &headers(&[]), now),
            secs(60),
            "no time given: a minute"
        );
        let secondary = headers(&[
            ("x-ratelimit-remaining", "40"),
            ("x-ratelimit-reset", "4000"),
        ]);
        assert_eq!(
            wait(429, &secondary, now),
            secs(60),
            "quota left: a minute, not the reset"
        );
        let spent = headers(&[("RateLimit-Remaining", "0"), ("RateLimit-Reset", "1020")]);
        assert_eq!(wait(429, &spent, now), secs(20));
        let past = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "900")]);
        assert_eq!(
            wait(403, &past, now),
            secs(1),
            "a reset already passed waits a second"
        );
    }

    #[test]
    fn wait_leaves_other_responses_alone() {
        let some_left = headers(&[
            ("x-ratelimit-remaining", "12"),
            ("x-ratelimit-reset", "1030"),
        ]);
        assert_eq!(
            wait(403, &some_left, 1_000),
            None,
            "a 403 with quota left is a denial"
        );
        assert_eq!(wait(403, &headers(&[]), 1_000), None);
        assert_eq!(wait(200, &headers(&[("Retry-After", "5")]), 1_000), None);
        assert_eq!(wait(500, &headers(&[]), 1_000), None);
    }

    fn client(server: &StubServer) -> ForgeClient {
        ForgeClient::new(
            server.base_url(),
            "tok".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        )
    }

    #[test]
    fn a_short_rate_limit_is_waited_out_and_the_request_resent() {
        let limited = route_with_headers("GET", "/x", 429, "{}", &[("Retry-After", "0")]);
        let server = StubServer::start(vec![limited, route("GET", "/x", 200, r#"{"ok":1}"#)]);
        let value = client(&server).get("x").expect("the second try succeeds");
        assert_eq!(value["ok"], 1);
        assert_eq!(server.request_lines(), vec!["GET /x", "GET /x"]);
    }

    #[test]
    fn writes_are_resent_after_a_rate_limit_too() {
        let limited = route_with_headers("POST", "/x", 429, "{}", &[("Retry-After", "0")]);
        let server = StubServer::start(vec![limited, route("POST", "/x", 201, "{}")]);
        client(&server)
            .post("x", &serde_json::json!({}))
            .expect("resent");
        assert_eq!(server.request_lines().len(), 2);
    }

    #[test]
    fn a_persistent_rate_limit_fails_after_the_retries() {
        let limited = route_with_headers("GET", "/x", 429, "slow", &[("Retry-After", "0")]);
        let server = StubServer::start(vec![limited]);
        let err = client(&server).get("x").expect_err("still limited");
        assert!(err.to_string().contains("HTTP 429"), "{err}");
        assert_eq!(server.request_lines().len(), 1 + MAX_RETRIES as usize);
    }

    #[test]
    fn a_long_rate_limit_is_reported_rather_than_slept() {
        let limited = route_with_headers("GET", "/x", 429, "slow", &[("Retry-After", "3600")]);
        let server = StubServer::start(vec![limited]);
        let err = client(&server).get("x").expect_err("not slept");
        assert!(err.to_string().contains("HTTP 429"), "{err}");
        assert_eq!(server.request_lines().len(), 1);
    }

    #[test]
    fn paginated_and_delete_requests_wait_out_a_rate_limit() {
        let limited = route_with_headers("GET", "/p", 429, "{}", &[("Retry-After", "0")]);
        let del = route_with_headers("DELETE", "/d", 429, "{}", &[("Retry-After", "0")]);
        let server = StubServer::start(vec![
            limited,
            route("GET", "/p", 200, "[1]"),
            del,
            route("DELETE", "/d", 204, ""),
        ]);
        let c = client(&server);
        assert_eq!(c.get_paginated("p").expect("resent").len(), 1);
        c.delete("d").expect("resent");
        assert_eq!(server.request_lines().len(), 4);
    }
}
