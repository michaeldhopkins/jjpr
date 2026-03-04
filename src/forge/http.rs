use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use ureq::http;

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
            anyhow::bail!("GET {path} failed (HTTP {status}): {}", truncate_body(&body, 500));
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
            anyhow::bail!("{method} {path} failed (HTTP {status}): {}", truncate_body(&resp_body, 500));
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
                anyhow::bail!("GET {path} failed (HTTP {status}): {}", truncate_body(&body, 500));
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

    /// POST to a GraphQL endpoint (GitHub-specific).
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
        self.post(endpoint, &body)
    }
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
