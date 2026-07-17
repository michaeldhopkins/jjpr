use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use ureq::http;

/// Structured HTTP error preserving status code for caller matching.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub method: String,
    pub path: String,
    pub body: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} failed (HTTP {}): {}",
            self.method,
            self.path,
            self.status,
            truncate_body(&self.body, 500)
        )
    }
}

impl std::error::Error for HttpError {}

/// How pagination works for a given forge.
#[derive(Debug, Clone, Copy)]
pub enum PaginationStyle {
    /// Follow `Link: <url>; rel="next"` headers (GitHub, GitLab).
    LinkHeader,
    /// Increment `?page=N&limit=L` until an empty page (Forgejo).
    PageNumber { limit: u32 },
}

/// How the token is sent in HTTP requests.
#[derive(Debug, Clone, Copy)]
pub enum AuthScheme {
    /// `Authorization: Bearer {token}` (GitHub)
    Bearer,
    /// `PRIVATE-TOKEN: {token}` (GitLab)
    PrivateToken,
    /// `Authorization: token {token}` (Forgejo)
    Token,
}

/// Shared HTTP client wrapping a `ureq::Agent` with auth, base URL, and pagination.
#[derive(Clone)]
pub struct ForgeClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
    auth_scheme: AuthScheme,
    pagination: PaginationStyle,
}

impl ForgeClient {
    pub fn new(
        base_url: &str,
        token: String,
        auth_scheme: AuthScheme,
        pagination: PaginationStyle,
    ) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            // ureq keeps 3 idle connections per host by default, which is fewer
            // than jjpr issues at once. The surplus would be closed on return and
            // re-handshaked (~90ms of TCP+TLS) on the next wave, so the pool is
            // sized to the fan-out. This caps reuse, not concurrency: ureq never
            // throttles in-flight requests on the pool.
            .max_idle_connections_per_host(crate::parallel::MAX_CONCURRENT_REQUESTS)
            .max_idle_connections(crate::parallel::MAX_CONCURRENT_REQUESTS)
            .build()
            .into();

        Self {
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            auth_scheme,
            pagination,
        }
    }

    fn auth_header(&self) -> (&str, String) {
        match self.auth_scheme {
            AuthScheme::Bearer => ("Authorization", format!("Bearer {}", self.token)),
            AuthScheme::PrivateToken => ("PRIVATE-TOKEN", self.token.clone()),
            AuthScheme::Token => ("Authorization", format!("token {}", self.token)),
        }
    }

    fn full_url(&self, path: &str) -> Result<String, String> {
        if path.starts_with("https://") || path.starts_with("http://") {
            if same_origin(&self.base_url, path) {
                Ok(path.to_string())
            } else {
                Err(format!(
                    "refusing to follow cross-origin URL: {path} (expected host from {})",
                    self.base_url
                ))
            }
        } else {
            Ok(format!("{}/{}", self.base_url, path.trim_start_matches('/')))
        }
    }

    /// GET a single JSON response.
    pub fn get(&self, path: &str) -> Result<serde_json::Value> {
        let url = self.full_url(path)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (header, value) = self.auth_header();
        let mut resp = self.agent.get(&url)
            .header(header, &value)
            .header("Accept", "application/json")
            .call()
            .with_context(|| format!("GET {url}"))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.body_mut().read_to_string()
                .unwrap_or_else(|_| String::from("<unreadable>"));
            return Err(HttpError {
                status,
                method: "GET".to_string(),
                path: path.to_string(),
                body,
            }.into());
        }

        resp.body_mut().read_json()
            .with_context(|| format!("failed to parse JSON from GET {path}"))
    }

    /// POST with a JSON body, return the response JSON.
    pub fn post(&self, path: &str, body: &impl Serialize) -> Result<serde_json::Value> {
        self.request_with_body("POST", path, body)
    }

    /// PATCH with a JSON body, return the response JSON.
    pub fn patch(&self, path: &str, body: &impl Serialize) -> Result<serde_json::Value> {
        self.request_with_body("PATCH", path, body)
    }

    /// PUT with a JSON body, return the response JSON.
    pub fn put(&self, path: &str, body: &impl Serialize) -> Result<serde_json::Value> {
        self.request_with_body("PUT", path, body)
    }

    fn request_with_body(
        &self,
        method: &str,
        path: &str,
        body: &impl Serialize,
    ) -> Result<serde_json::Value> {
        let url = self.full_url(path)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (header, value) = self.auth_header();

        let request = match method {
            "POST" => self.agent.post(&url),
            "PATCH" => self.agent.patch(&url),
            "PUT" => self.agent.put(&url),
            _ => anyhow::bail!("unsupported HTTP method: {method}"),
        };

        let mut resp = request
            .header(header, &value)
            .header("Accept", "application/json")
            .send_json(body)
            .with_context(|| format!("{method} {url}"))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let resp_body = resp.body_mut().read_to_string()
                .unwrap_or_else(|_| String::from("<unreadable>"));
            return Err(HttpError {
                status,
                method: method.to_string(),
                path: path.to_string(),
                body: resp_body,
            }.into());
        }

        // Some endpoints return 204 No Content or empty body on success
        if status == 204 {
            return Ok(serde_json::Value::Null);
        }

        let text = resp.body_mut().read_to_string()
            .with_context(|| format!("failed to read response from {method} {path}"))?;
        if text.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse JSON from {method} {path}"))
    }

    /// GET with pagination, collecting all pages into a single Vec.
    pub fn get_paginated(&self, path: &str) -> Result<Vec<serde_json::Value>> {
        match self.pagination {
            PaginationStyle::LinkHeader => self.get_paginated_link(path),
            PaginationStyle::PageNumber { limit } => self.get_paginated_page(path, limit),
        }
    }

    fn get_paginated_link(&self, path: &str) -> Result<Vec<serde_json::Value>> {
        const MAX_PAGES: usize = 100;

        let mut url = self.full_url(path)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (header, value) = self.auth_header();
        let mut all_items = Vec::new();

        for _ in 0..MAX_PAGES {
            let mut resp = self.agent.get(&url)
                .header(header, &value)
                .header("Accept", "application/json")
                .call()
                .with_context(|| format!("GET {url}"))?;

            let status = resp.status().as_u16();
            if status >= 400 {
                let body = resp.body_mut().read_to_string()
                    .unwrap_or_else(|_| String::from("<unreadable>"));
                return Err(HttpError {
                    status,
                    method: "GET".to_string(),
                    path: path.to_string(),
                    body,
                }.into());
            }

            let next = extract_next_link(&resp);

            let items: Vec<serde_json::Value> = resp.body_mut().read_json()
                .with_context(|| format!("failed to parse paginated JSON from GET {path}"))?;
            all_items.extend(items);

            match next {
                Some(next_url) => {
                    url = self.full_url(&next_url)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                None => return Ok(all_items),
            }
        }

        eprintln!("warning: pagination capped at {MAX_PAGES} pages for {path}");
        Ok(all_items)
    }

    /// GET every page of an endpoint that wraps its array in an object, and
    /// concatenate the arrays found at `key`.
    ///
    /// GitHub's check-runs and combined-status endpoints return
    /// `{"total_count": N, "check_runs": [...]}` rather than a bare array, so
    /// [`Self::get_paginated`] cannot read them. Without this, only the first
    /// page is ever seen and a repo with more checks than fit on one page
    /// reports a status derived from an arbitrary subset.
    pub fn get_paginated_envelope(
        &self,
        path: &str,
        key: &str,
    ) -> Result<Vec<serde_json::Value>> {
        const MAX_PAGES: usize = 100;

        let mut url = self
            .full_url(path)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (header, value) = self.auth_header();
        let mut all_items = Vec::new();

        for _ in 0..MAX_PAGES {
            let mut resp = self
                .agent
                .get(&url)
                .header(header, &value)
                .header("Accept", "application/json")
                .call()
                .with_context(|| format!("GET {url}"))?;

            let status = resp.status().as_u16();
            if status >= 400 {
                let body = resp
                    .body_mut()
                    .read_to_string()
                    .unwrap_or_else(|_| String::from("<unreadable>"));
                return Err(HttpError {
                    status,
                    method: "GET".to_string(),
                    path: path.to_string(),
                    body,
                }
                .into());
            }

            let next = extract_next_link(&resp);

            let page: serde_json::Value = resp
                .body_mut()
                .read_json()
                .with_context(|| format!("failed to parse paginated JSON from GET {path}"))?;
            all_items.extend(envelope_items(&page, key));

            match next {
                Some(next_url) => {
                    url = self
                        .full_url(&next_url)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                None => return Ok(all_items),
            }
        }

        eprintln!("warning: pagination capped at {MAX_PAGES} pages for {path}");
        Ok(all_items)
    }

    fn get_paginated_page(&self, path: &str, limit: u32) -> Result<Vec<serde_json::Value>> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut all_items = Vec::new();
        let mut page = 1u32;

        loop {
            let paged_path = format!("{path}{separator}page={page}&limit={limit}");
            let items: Vec<serde_json::Value> = self.get(&paged_path)
                .and_then(|v| serde_json::from_value(v)
                    .context("failed to parse paginated response"))?;

            let count = items.len();
            if count == 0 {
                break;
            }
            all_items.extend(items);
            // A partial page means we've reached the end
            if (count as u32) < limit {
                break;
            }
            page += 1;
        }

        Ok(all_items)
    }

    /// POST to a GraphQL endpoint (GitHub-specific), returning the `data` object.
    ///
    /// GraphQL reports almost every failure as HTTP 200 with a top-level
    /// `errors` array, so a status-code check alone sees a permission denial or
    /// a malformed query as success. Only transport-level failures (a rejected
    /// token is 401) surface as an [`HttpError`]. This checks both, and treats
    /// any `errors` entry as a failure even when partial `data` came back —
    /// a half-populated result would silently understate a PR's real status.
    pub fn graphql(
        &self,
        endpoint: &str,
        query: &str,
        variables: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });
        let response = self.post(endpoint, &body)?;

        if let Some(errors) = response.get("errors").and_then(|e| e.as_array())
            && !errors.is_empty()
        {
            return Err(GraphQlError::from_errors(errors).into());
        }

        response
            .get("data")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("GraphQL response had neither data nor errors"))
    }
}

/// A GraphQL request that failed inside an HTTP 200 response.
///
/// Callers are expected to treat any of these as "fall back to REST". The type
/// is kept for diagnostics rather than for branching: GitHub does not publish
/// an exhaustive list of `type` values, and it deliberately reports a repo the
/// token cannot read as `NOT_FOUND` rather than `FORBIDDEN` so as not to leak
/// whether the repo exists — so `NOT_FOUND` cannot be read as "really absent".
#[derive(Debug)]
pub struct GraphQlError {
    /// The `type` GitHub attaches to each error: `NOT_FOUND`, `FORBIDDEN`,
    /// `RATE_LIMITED`, `INSUFFICIENT_SCOPES`. Query-level mistakes (a bad field
    /// name) carry no type at all, so this can legitimately be empty.
    pub types: Vec<String>,
    pub message: String,
}

impl GraphQlError {
    fn from_errors(errors: &[serde_json::Value]) -> Self {
        let types: Vec<String> = errors
            .iter()
            .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
            .map(str::to_string)
            .collect();
        let message = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
            .collect::<Vec<_>>()
            .join("; ");
        Self {
            types,
            message: if message.is_empty() {
                String::from("<no message>")
            } else {
                message
            },
        }
    }

}

impl fmt::Display for GraphQlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.types.is_empty() {
            write!(f, "GraphQL error: {}", truncate_body(&self.message, 500))
        } else {
            write!(
                f,
                "GraphQL error [{}]: {}",
                self.types.join(", "),
                truncate_body(&self.message, 500)
            )
        }
    }
}

impl std::error::Error for GraphQlError {}

/// Pull the array at `key` out of one envelope page.
///
/// A missing or non-array key yields nothing rather than an error, matching how
/// the check parsers already treat an absent array: as "none reported". Erroring
/// instead would turn an unfamiliar payload into a failed command.
fn envelope_items(page: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    page.get(key)
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Truncate a string to a maximum byte length, appending "…" if truncated.
fn truncate_body(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Walk backwards from max to find a char boundary
        let mut boundary = max;
        while boundary > 0 && !s.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}…", &s[..boundary])
    }
}

/// Percent-encode a string for safe use in URL path segments or query values.
pub fn url_encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Check whether two URLs share the same scheme + host + port.
fn same_origin(base: &str, candidate: &str) -> bool {
    let extract = |url: &str| -> Option<(String, String)> {
        let after_scheme = url.split("://").nth(1)?;
        let scheme = url.split("://").next()?;
        let host_port = after_scheme.split('/').next()?;
        Some((scheme.to_lowercase(), host_port.to_lowercase()))
    };
    match (extract(base), extract(candidate)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Extract the `rel="next"` URL from a Link header.
fn extract_next_link(resp: &http::Response<ureq::Body>) -> Option<String> {
    let link = resp.headers().get("link")?.to_str().ok()?;
    for part in link.split(',') {
        let part: &str = part.trim();
        if part.contains("rel=\"next\"") {
            let url = part.split('<').nth(1)?
                .split('>').next()?;
            return Some(url.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_pool_is_sized_to_the_fan_out() {
        // ureq's default keeps 3 idle connections per host. Left there, every
        // wave past the third concurrent request would re-handshake TCP+TLS.
        // This pins the pool to the fan-out so raising one cannot silently
        // leave the other behind.
        let client = ForgeClient::new(
            "https://api.example.com",
            "token".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        let config = client.agent.config();
        assert_eq!(
            config.max_idle_connections_per_host(),
            crate::parallel::MAX_CONCURRENT_REQUESTS,
        );
        assert!(config.max_idle_connections() >= crate::parallel::MAX_CONCURRENT_REQUESTS);
    }

    // GraphQL reports permission and rate-limit failures inside an HTTP 200, so
    // these pin the shapes that must not be mistaken for success. The bodies are
    // taken from live responses.
    #[test]
    fn graphql_error_captures_type_and_message() {
        let errors = vec![serde_json::json!({
            "type": "NOT_FOUND",
            "message": "Could not resolve to a Repository with the name 'a/b'.",
        })];
        let err = GraphQlError::from_errors(&errors);
        assert_eq!(err.types, vec!["NOT_FOUND"]);
        assert!(err.message.contains("Could not resolve"));
        assert!(err.to_string().contains("NOT_FOUND"));
    }

    #[test]
    fn graphql_saml_denial_is_a_forbidden_error() {
        let errors = vec![serde_json::json!({
            "type": "FORBIDDEN",
            "message": "Resource protected by organization SAML enforcement.",
            "extensions": { "saml_failure": true },
        })];
        let err = GraphQlError::from_errors(&errors);
        assert_eq!(err.types, vec!["FORBIDDEN"]);
    }

    #[test]
    fn graphql_syntax_errors_carry_no_type() {
        // A malformed query returns errors with no `type` at all. It must still
        // be recognized as a failure rather than parsed as data.
        let errors = vec![serde_json::json!({
            "message": "Field 'nope' doesn't exist on type 'PullRequest'",
        })];
        let err = GraphQlError::from_errors(&errors);
        assert!(err.types.is_empty());
        assert!(err.to_string().contains("doesn't exist"));
    }

    #[test]
    fn graphql_error_without_message_still_displays() {
        let err = GraphQlError::from_errors(&[serde_json::json!({ "type": "RATE_LIMITED" })]);
        assert!(err.to_string().contains("RATE_LIMITED"));
        assert!(err.to_string().contains("<no message>"));
    }

    #[test]
    fn graphql_error_joins_multiple_entries() {
        let errors = vec![
            serde_json::json!({"type": "NOT_FOUND", "message": "first"}),
            serde_json::json!({"type": "FORBIDDEN", "message": "second"}),
        ];
        let err = GraphQlError::from_errors(&errors);
        assert_eq!(err.types, vec!["NOT_FOUND", "FORBIDDEN"]);
        assert!(err.message.contains("first") && err.message.contains("second"));
    }

    // GitHub wraps check-runs and combined statuses in an envelope rather than
    // returning a bare array, which is why these endpoints need their own
    // paginator.
    #[test]
    fn envelope_items_reads_the_named_array() {
        let page = serde_json::json!({
            "total_count": 2,
            "check_runs": [{"conclusion": "success"}, {"conclusion": "failure"}],
        });
        assert_eq!(envelope_items(&page, "check_runs").len(), 2);
    }

    #[test]
    fn envelope_items_reads_the_statuses_shape_too() {
        let page = serde_json::json!({ "state": "success", "statuses": [{"state": "success"}] });
        assert_eq!(envelope_items(&page, "statuses").len(), 1);
    }

    #[test]
    fn envelope_items_treats_a_missing_or_odd_key_as_empty() {
        // Matches how the check parsers read an absent array: none reported,
        // rather than a failed command.
        assert!(envelope_items(&serde_json::json!({"total_count": 0}), "check_runs").is_empty());
        assert!(envelope_items(&serde_json::json!({"check_runs": null}), "check_runs").is_empty());
        assert!(envelope_items(&serde_json::json!({"check_runs": "nope"}), "check_runs").is_empty());
        assert!(envelope_items(&serde_json::json!([]), "check_runs").is_empty());
    }

    #[test]
    fn test_extract_next_link_present() {
        let resp = http::Response::builder()
            .header("link", r#"<https://api.github.com/repos?page=2>; rel="next", <https://api.github.com/repos?page=5>; rel="last""#)
            .body(ureq::Body::builder().data(""))
            .expect("build response");

        let next = extract_next_link(&resp);
        assert_eq!(next.as_deref(), Some("https://api.github.com/repos?page=2"));
    }

    #[test]
    fn test_extract_next_link_absent() {
        let resp = http::Response::builder()
            .header("link", r#"<https://api.github.com/repos?page=5>; rel="last""#)
            .body(ureq::Body::builder().data(""))
            .expect("build response");

        let next = extract_next_link(&resp);
        assert!(next.is_none());
    }

    #[test]
    fn test_extract_next_link_no_header() {
        let resp = http::Response::builder()
            .body(ureq::Body::builder().data(""))
            .expect("build response");

        let next = extract_next_link(&resp);
        assert!(next.is_none());
    }

    #[test]
    fn test_full_url_relative() {
        let client = ForgeClient::new(
            "https://api.github.com",
            "tok".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        assert_eq!(client.full_url("repos/o/r").unwrap(), "https://api.github.com/repos/o/r");
        assert_eq!(client.full_url("/repos/o/r").unwrap(), "https://api.github.com/repos/o/r");
    }

    #[test]
    fn test_full_url_absolute_same_origin() {
        let client = ForgeClient::new(
            "https://api.github.com",
            "tok".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        assert_eq!(
            client.full_url("https://api.github.com/repos?page=2").unwrap(),
            "https://api.github.com/repos?page=2"
        );
    }

    #[test]
    fn test_full_url_rejects_cross_origin() {
        let client = ForgeClient::new(
            "https://api.github.com",
            "tok".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        let result = client.full_url("https://evil.example.com/steal");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cross-origin"));
    }

    #[test]
    fn test_same_origin_matching() {
        assert!(same_origin("https://api.github.com/v3", "https://api.github.com/repos?page=2"));
        assert!(same_origin("https://gitlab.com/api/v4", "https://gitlab.com/other"));
    }

    #[test]
    fn test_same_origin_different_host() {
        assert!(!same_origin("https://api.github.com", "https://evil.com/steal"));
    }

    #[test]
    fn test_same_origin_different_scheme() {
        assert!(!same_origin("https://api.github.com", "http://api.github.com/repos"));
    }

    #[test]
    fn test_same_origin_with_port() {
        assert!(same_origin("https://gitlab.local:8443/api", "https://gitlab.local:8443/v2"));
        assert!(!same_origin("https://gitlab.local:8443/api", "https://gitlab.local:9999/v2"));
    }

    #[test]
    fn test_url_encode_slash() {
        assert_eq!(url_encode("feature/login"), "feature%2Flogin");
    }

    #[test]
    fn test_url_encode_hash() {
        assert_eq!(url_encode("fix#123"), "fix%23123");
    }

    #[test]
    fn test_url_encode_plain() {
        assert_eq!(url_encode("auth"), "auth");
    }

    #[test]
    fn test_url_encode_space() {
        assert_eq!(url_encode("my feature"), "my%20feature");
    }

    #[test]
    fn test_truncate_body_short() {
        assert_eq!(truncate_body("hello", 500), "hello");
    }

    #[test]
    fn test_truncate_body_long() {
        let long = "x".repeat(600);
        let result = truncate_body(&long, 500);
        assert!(result.len() < 510);
        assert!(result.ends_with('…'));
        assert!(result.starts_with("xxx"));
    }

    #[test]
    fn test_auth_header_bearer() {
        let client = ForgeClient::new(
            "https://api.github.com",
            "ghp_xxx".to_string(),
            AuthScheme::Bearer,
            PaginationStyle::LinkHeader,
        );
        let (name, value) = client.auth_header();
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer ghp_xxx");
    }

    #[test]
    fn test_auth_header_private_token() {
        let client = ForgeClient::new(
            "https://gitlab.com/api/v4",
            "glpat_xxx".to_string(),
            AuthScheme::PrivateToken,
            PaginationStyle::LinkHeader,
        );
        let (name, value) = client.auth_header();
        assert_eq!(name, "PRIVATE-TOKEN");
        assert_eq!(value, "glpat_xxx");
    }

    #[test]
    fn test_auth_header_token() {
        let client = ForgeClient::new(
            "https://codeberg.org/api/v1",
            "tok_abc".to_string(),
            AuthScheme::Token,
            PaginationStyle::PageNumber { limit: 50 },
        );
        let (name, value) = client.auth_header();
        assert_eq!(name, "Authorization");
        assert_eq!(value, "token tok_abc");
    }
}
