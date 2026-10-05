//! Web search and document fetching, ported from
//! `mcp-castor/src/harness/services/web_service.js`.
//!
//! `web_search` runs a strict provider chain:
//!   1. SearXNG JSON API (only when `searxng_url` is configured). If it is
//!      configured but unreachable / returns an HTTP error, we surface a
//!      *typed, actionable* error naming what to set or run — we do NOT
//!      silently fall through to another provider.
//!   2. Brave Search API (only when `brave_api_key` is configured).
//!   3. DuckDuckGo HTML endpoint (last resort, no key required).
//!
//! `fetch_docs` performs a GET, gates on the `Content-Type` header (plus a
//! binary sniff for unknown types), converts HTML to Markdown via `html2md`,
//! passes JSON / plain text through, fails fast on binary content, caps the
//! output at ~20KB, and returns verbatim HTTP errors.
//!
//! All HTTP goes through a single `reqwest` client (rustls) with short
//! timeouts. The provider base URLs are injectable so the test suite can
//! point them at local axum mocks (no network).

use thiserror::Error;

/// Default base URLs (overridable in tests via [`WebClient::with_base_urls`]).
pub const DEFAULT_SEARXNG: Option<&'static str> = Some("http://127.0.0.1:8888");
const DEFAULT_BRAVE: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_DDG: &str = "https://html.duckduckgo.com/html/";

/// Short timeout applied to every outbound request.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Output cap for [`WebClient::fetch_docs`] (~20KB).
const MAX_OUTPUT_BYTES: usize = 20_000;

/// User-Agent sent on every request.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 Castor/0.1.0";

/// Typed, actionable web errors.
#[derive(Debug, Error)]
pub enum WebError {
    #[error("InvalidQueryError: search query must be a non-empty string")]
    InvalidQuery,
    #[error("InvalidUrlError: only http/https URLs are supported (got '{0}')")]
    InvalidUrl(String),
    #[error(
        "SearxngNotConfiguredError: no SearXNG instance is configured. Set `searxng_url` in your castor config (or the CASTOR_SEARXNG_URL env var) to a running SearXNG instance, e.g. http://127.0.0.1:8888, or provide a `brave_api_key` to use the Brave provider."
    )]
    SearxngNotConfigured,
    #[error(
        "SearxngUnreachableError: SearXNG at {url} is unreachable ({reason}). Verify the instance is running (e.g. `docker start anser-searxng` or `docker compose up -d`) and that `searxng_url` points at it, or configure a `brave_api_key` to fall back to Brave."
    )]
    SearxngUnreachable { url: String, reason: String },
    #[error("BraveSearchError: {0}")]
    Brave(String),
    #[error("HttpError: {method} '{url}' failed with status {status} {reason}")]
    Http {
        method: String,
        url: String,
        status: u16,
        reason: String,
    },
    #[error(
        "BinaryContentError: '{url}' returned binary content ({content_type}, {byte_length} bytes); not rendered as text"
    )]
    Binary {
        url: String,
        content_type: String,
        byte_length: usize,
    },
}

/// A single normalized search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// The outcome of a [`WebClient::web_search`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub query: String,
    pub provider: String,
    pub results: Vec<SearchResult>,
}

/// The outcome of a [`WebClient::fetch_docs`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResult {
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub markdown: String,
    /// Length of the full (pre-truncation) content.
    pub length: usize,
}

/// A web client with injectable provider base URLs (for tests).
#[derive(Clone)]
pub struct WebClient {
    client: reqwest::Client,
    searxng_base: Option<String>,
    brave_base: String,
    ddg_base: String,
}

impl WebClient {
    /// Build a client with the default (real) provider base URLs.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client");
        Self {
            client,
            searxng_base: DEFAULT_SEARXNG.map(str::to_string),
            brave_base: DEFAULT_BRAVE.to_string(),
            ddg_base: DEFAULT_DDG.to_string(),
        }
    }

    /// Build a client with overridden provider base URLs (used by the test
    /// suite to point at local axum mocks).
    pub fn with_base_urls(
        searxng: Option<String>,
        brave: Option<String>,
        ddg: Option<String>,
    ) -> Self {
        let mut c = Self::new();
        c.searxng_base = searxng;
        if let Some(b) = brave {
            c.brave_base = b;
        }
        if let Some(d) = ddg {
            c.ddg_base = d;
        }
        c
    }
}

impl Default for WebClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

impl WebClient {
    /// Run the strict provider chain: SearXNG → Brave → DuckDuckGo.
    pub async fn web_search(
        &self,
        query: &str,
        brave_api_key: Option<&str>,
    ) -> Result<SearchOutcome, WebError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(WebError::InvalidQuery);
        }

        // 1. SearXNG (only when configured). A configured-but-failing instance
        //    is a hard, actionable error — no silent fallthrough.
        if let Some(base) = self.searxng_base.as_deref() {
            match self.search_searxng(q, base).await {
                Ok(results) => {
                    return Ok(SearchOutcome {
                        query: q.to_string(),
                        provider: "searxng".into(),
                        results,
                    });
                }
                Err(e) => return Err(e),
            }
        }

        // 2. Brave (only when a key is configured).
        if let Some(key) = brave_api_key
            && !key.is_empty()
        {
            let results = self.search_brave(q, key).await?;
            return Ok(SearchOutcome {
                query: q.to_string(),
                provider: "brave".into(),
                results,
            });
        }

        // 3. DuckDuckGo HTML (last resort).
        let results = self.search_ddg(q).await?;
        Ok(SearchOutcome {
            query: q.to_string(),
            provider: "duckduckgo".into(),
            results,
        })
    }

    async fn search_searxng(&self, q: &str, base: &str) -> Result<Vec<SearchResult>, WebError> {
        let base = base.trim_end_matches('/');
        let mut url = reqwest::Url::parse(&format!("{base}/search")).map_err(|e| {
            WebError::SearxngUnreachable {
                url: base.to_string(),
                reason: format!("invalid base url: {e}"),
            }
        })?;
        url.query_pairs_mut()
            .append_pair("q", q)
            .append_pair("format", "json");

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| WebError::SearxngUnreachable {
                url: base.to_string(),
                reason: e.to_string(),
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(WebError::SearxngUnreachable {
                url: base.to_string(),
                reason: format!("HTTP {status}"),
            });
        }

        let json: serde_json::Value =
            resp.json()
                .await
                .map_err(|e| WebError::SearxngUnreachable {
                    url: base.to_string(),
                    reason: format!("invalid JSON: {e}"),
                })?;

        let mut out = Vec::new();
        if let Some(items) = json.get("results").and_then(|v| v.as_array()) {
            for it in items {
                let title = it.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let url = it.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let snippet = it.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !url.is_empty() {
                    out.push(SearchResult {
                        title: title.to_string(),
                        url: url.to_string(),
                        snippet: snippet.to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn search_brave(&self, q: &str, key: &str) -> Result<Vec<SearchResult>, WebError> {
        let mut url = reqwest::Url::parse(&self.brave_base)
            .map_err(|e| WebError::Brave(format!("invalid base url: {e}")))?;
        url.query_pairs_mut()
            .append_pair("q", q)
            .append_pair("count", "10");

        let resp = self
            .client
            .get(url)
            .header("X-Subscription-Token", key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| WebError::Brave(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(WebError::Brave(format!("HTTP {status}")));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| WebError::Brave(format!("invalid JSON: {e}")))?;

        let mut out = Vec::new();
        if let Some(items) = json
            .get("web")
            .and_then(|v| v.get("results"))
            .and_then(|v| v.as_array())
        {
            for it in items {
                let title = it.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let url = it.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let snippet = it.get("description").and_then(|v| v.as_str()).unwrap_or("");
                if !url.is_empty() {
                    out.push(SearchResult {
                        title: title.to_string(),
                        url: url.to_string(),
                        snippet: snippet.to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn search_ddg(&self, q: &str) -> Result<Vec<SearchResult>, WebError> {
        let body = format!("q={}", percent_encode(q));
        let resp = self
            .client
            .post(&self.ddg_base)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| WebError::Http {
                method: "POST".into(),
                url: self.ddg_base.clone(),
                status: 0,
                reason: e.to_string(),
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(WebError::Http {
                method: "POST".into(),
                url: self.ddg_base.clone(),
                status: status.as_u16(),
                reason: status.canonical_reason().unwrap_or("").to_string(),
            });
        }

        let html = resp.text().await.map_err(|e| WebError::Http {
            method: "POST".into(),
            url: self.ddg_base.clone(),
            status: 0,
            reason: e.to_string(),
        })?;

        Ok(parse_ddg_html(&html))
    }
}

/// Parse the DuckDuckGo HTML endpoint into normalized results.
fn parse_ddg_html(html: &str) -> Vec<SearchResult> {
    let doc = scraper::Html::parse_document(html);
    let result_sel = match scraper::Selector::parse(".result") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let title_sel = match scraper::Selector::parse(".result__title a") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let snippet_sel = match scraper::Selector::parse(".result__snippet") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for el in doc.select(&result_sel) {
        let title_el = match el.select(&title_sel).next() {
            Some(t) => t,
            None => continue,
        };
        let title = title_el.text().collect::<String>().trim().to_string();
        let raw_link = title_el.attr("href").unwrap_or("").to_string();
        let link = resolve_ddg_link(&raw_link);
        let snippet = el
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        if !title.is_empty() && !link.is_empty() {
            out.push(SearchResult {
                title,
                url: link,
                snippet,
            });
        }
    }
    out
}

/// DuckDuckGo wraps the real URL in a `uddg` query parameter on its own
/// redirect endpoint; decode it when present.
fn resolve_ddg_link(link: &str) -> String {
    if let Ok(u) = reqwest::Url::parse(link)
        && let Some(uddg) = u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v)
        && !uddg.is_empty()
    {
        return uddg.to_string();
    }
    link.to_string()
}

/// Percent-encode a string for use in a URL query / form body.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

impl WebClient {
    /// Fetch a URL and convert it to Markdown (or pass JSON / text through).
    pub async fn fetch_docs(&self, url: &str) -> Result<FetchResult, WebError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| WebError::InvalidUrl(url.to_string()))?;
        let scheme = parsed.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(WebError::InvalidUrl(scheme.to_string()));
        }

        let resp = self
            .client
            .get(parsed.clone())
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/json;q=0.8,text/plain;q=0.7,*/*;q=0.5",
            )
            .send()
            .await
            .map_err(|e| WebError::Http {
                method: "GET".into(),
                url: url.to_string(),
                status: 0,
                reason: e.to_string(),
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Err(WebError::Http {
                method: "GET".into(),
                url: url.to_string(),
                status: status.as_u16(),
                reason: status.canonical_reason().unwrap_or("").to_string(),
            });
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| WebError::Http {
                method: "GET".into(),
                url: url.to_string(),
                status: 0,
                reason: e.to_string(),
            })?
            .to_vec();

        let category = classify(&content_type, &bytes);
        let (out_type, markdown, length) = match category {
            Category::Html => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let md = html2md::parse_html(&text);
                ("text/html".to_string(), md, 0)
            }
            Category::Json => {
                let v: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|e| WebError::Http {
                        method: "GET".into(),
                        url: url.to_string(),
                        status: 0,
                        reason: format!("invalid JSON: {e}"),
                    })?;
                let s = serde_json::to_string_pretty(&v).unwrap_or_default();
                let len = s.len();
                ("application/json".to_string(), s, len)
            }
            Category::Text => {
                let s = String::from_utf8_lossy(&bytes).into_owned();
                (content_type.clone(), s, 0)
            }
            Category::Binary => {
                return Err(WebError::Binary {
                    url: url.to_string(),
                    content_type: if content_type.is_empty() {
                        "application/octet-stream".into()
                    } else {
                        content_type
                    },
                    byte_length: bytes.len(),
                });
            }
        };

        let full_len = if length == 0 { markdown.len() } else { length };
        let markdown = cap_output(&markdown, full_len);

        Ok(FetchResult {
            url: url.to_string(),
            status: status.as_u16(),
            content_type: out_type,
            markdown,
            length: full_len,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Html,
    Json,
    Text,
    Binary,
}

/// Classify a response body from its Content-Type header plus a binary sniff.
fn classify(content_type: &str, bytes: &[u8]) -> Category {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    match base.as_str() {
        "text/html" | "application/xhtml+xml" => return Category::Html,
        "application/json" => return Category::Json,
        _ => {}
    }
    if base.ends_with("+json") {
        return Category::Json;
    }
    if base.starts_with("text/") {
        return Category::Text;
    }
    if base.starts_with("image/")
        || base.starts_with("audio/")
        || base.starts_with("video/")
        || base == "application/octet-stream"
        || base == "application/zip"
        || base == "application/gzip"
        || base == "application/pdf"
    {
        return Category::Binary;
    }
    // Unknown / absent Content-Type: sniff the leading bytes for a NUL.
    if has_nul_byte(bytes) {
        return Category::Binary;
    }
    Category::Html
}

/// A NUL byte in the first 8KB is a strong signal of binary content.
fn has_nul_byte(bytes: &[u8]) -> bool {
    let limit = bytes.len().min(8192);
    bytes[..limit].contains(&0)
}

/// Truncate to the output cap, cutting at a clean boundary and appending a
/// marker reporting the shown and total sizes.
fn cap_output(text: &str, full_len: usize) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let window = &text[..MAX_OUTPUT_BYTES];
    let search_start = MAX_OUTPUT_BYTES.saturating_sub(500);
    let mut cut = window.rfind("\n\n").unwrap_or(0);
    if cut < search_start {
        cut = window.rfind('\n').unwrap_or(0);
    }
    if cut < search_start {
        cut = MAX_OUTPUT_BYTES;
    }
    let shown = &text[..cut];
    let mut out = shown.to_string();
    // Close an open markdown code fence so the document stays well-formed.
    let fence_count = out.lines().filter(|l| l.starts_with("```")).count();
    if fence_count % 2 == 1 {
        out = format!("{out}\n```");
    }
    out.push_str(&format!(
        "\n\n[Content truncated: showing {cut} of {full_len} chars]"
    ));
    out
}

// ---------------------------------------------------------------------------
// Tests (axum mocks on ephemeral ports, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    async fn start(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn json_body(s: &str) -> axum::response::Response {
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            s.to_string(),
        )
            .into_response()
    }

    fn html_body(s: &str) -> axum::response::Response {
        (
            StatusCode::OK,
            [("content-type", "text/html")],
            s.to_string(),
        )
            .into_response()
    }

    async fn searxng_handler() -> axum::response::Response {
        json_body(
            r#"{"results":[{"title":"SearXNG Result","url":"https://searx.example/a","content":"sx snippet"}]}"#,
        )
    }

    async fn brave_handler() -> axum::response::Response {
        json_body(
            r#"{"web":{"results":[{"title":"Brave Result","url":"https://brave.example/b","description":"brave snippet"}]}}"#,
        )
    }

    async fn ddg_handler() -> axum::response::Response {
        html_body(
            r#"<html><body>
<div class="result">
  <span class="result__title"><a href="https://ddg.example/c">DDG Result</a></span>
  <span class="result__snippet">ddg snippet</span>
</div>
</body></html>"#,
        )
    }

    async fn fetch_html_handler() -> axum::response::Response {
        html_body("<html><body><h1>Hello</h1><p>World</p></body></html>")
    }

    async fn fetch_404_handler() -> axum::response::Response {
        (StatusCode::NOT_FOUND, "Not Found").into_response()
    }

    async fn fetch_binary_handler() -> axum::response::Response {
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(b"PK\x03\x04");
        b.push(0);
        b.extend_from_slice(b"binary");
        (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            b,
        )
            .into_response()
    }

    async fn fetch_json_handler() -> axum::response::Response {
        json_body(r#"{"a":1,"b":"two"}"#)
    }

    async fn fetch_big_handler() -> axum::response::Response {
        let para = "lorem ipsum dolor sit amet ";
        let mut body = String::from("<html><body>");
        for _ in 0..1000 {
            body.push_str(&format!("<p>{para}</p>"));
        }
        body.push_str("</body></html>");
        html_body(&body)
    }

    fn router() -> Router {
        Router::new()
            .route("/searxng/search", get(searxng_handler))
            .route("/brave", get(brave_handler))
            .route("/ddg", post(ddg_handler))
            .route("/fetch/html", get(fetch_html_handler))
            .route("/fetch/404", get(fetch_404_handler))
            .route("/fetch/binary", get(fetch_binary_handler))
            .route("/fetch/json", get(fetch_json_handler))
            .route("/fetch/big", get(fetch_big_handler))
    }

    #[tokio::test]
    async fn searxng_results() {
        let base = start(router()).await;
        let c = WebClient::with_base_urls(Some(format!("{base}/searxng")), None, None);
        let out = c.web_search("rust async", None).await.unwrap();
        assert_eq!(out.provider, "searxng");
        assert_eq!(out.query, "rust async");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "SearXNG Result");
        assert_eq!(out.results[0].url, "https://searx.example/a");
        assert_eq!(out.results[0].snippet, "sx snippet");
    }

    #[tokio::test]
    async fn searxng_down_typed_actionable_error() {
        // SearXNG configured but the instance is down (connection refused).
        // Deterministically obtain a genuinely-closed port: bind a listener
        // on 127.0.0.1:0, read its port, drop it, then connect to it.
        // (127.0.0.1:1 can hang until the client timeout on some hosts.)
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let expected_url = format!("http://127.0.0.1:{port}");
        let c = WebClient::with_base_urls(Some(expected_url.clone()), None, None);
        let err = c.web_search("rust", None).await.unwrap_err();
        match &err {
            WebError::SearxngUnreachable { url, reason } => {
                assert_eq!(url, &expected_url);
                assert!(!reason.is_empty(), "reason must be non-empty");
            }
            other => panic!("expected SearxngUnreachable, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("SearxngUnreachableError"), "{msg}");
        assert!(
            msg.contains("searxng_url"),
            "must name the config key: {msg}"
        );
        assert!(
            msg.contains("brave_api_key"),
            "must name the alternative: {msg}"
        );
    }

    #[tokio::test]
    async fn brave_with_key_path() {
        let base = start(router()).await;
        let c = WebClient::with_base_urls(None, Some(format!("{base}/brave")), None);
        let out = c.web_search("tokio", Some("bkey")).await.unwrap();
        assert_eq!(out.provider, "brave");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "Brave Result");
        assert_eq!(out.results[0].url, "https://brave.example/b");
        assert_eq!(out.results[0].snippet, "brave snippet");
    }

    #[tokio::test]
    async fn ddg_fallback() {
        let base = start(router()).await;
        let c = WebClient::with_base_urls(None, None, Some(format!("{base}/ddg")));
        let out = c.web_search("axum", None).await.unwrap();
        assert_eq!(out.provider, "duckduckgo");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "DDG Result");
        assert_eq!(out.results[0].url, "https://ddg.example/c");
        assert_eq!(out.results[0].snippet, "ddg snippet");
    }

    #[tokio::test]
    async fn unconfigured_searxng_falls_to_ddg() {
        // No SearXNG configured and no Brave key → straight to DuckDuckGo.
        let base = start(router()).await;
        let c = WebClient::with_base_urls(None, None, Some(format!("{base}/ddg")));
        let out = c.web_search("serde", None).await.unwrap();
        assert_eq!(out.provider, "duckduckgo");
        assert!(!out.results.is_empty());
    }

    #[tokio::test]
    async fn html_to_markdown_via_html2md() {
        let base = start(router()).await;
        let c = WebClient::new();
        let r = c.fetch_docs(&format!("{base}/fetch/html")).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "text/html");
        // html2md renders <h1> as a setext heading (text + "====" underline).
        assert!(r.markdown.contains("Hello"), "got: {}", r.markdown);
        assert!(r.markdown.contains("World"), "got: {}", r.markdown);
        assert!(
            r.markdown.contains("===="),
            "expected setext underline, got: {}",
            r.markdown
        );
    }

    #[tokio::test]
    async fn fetch_404_verbatim() {
        let base = start(router()).await;
        let c = WebClient::new();
        let err = c
            .fetch_docs(&format!("{base}/fetch/404"))
            .await
            .unwrap_err();
        match &err {
            WebError::Http {
                method,
                status,
                reason,
                ..
            } => {
                assert_eq!(*method, "GET");
                assert_eq!(*status, 404);
                assert_eq!(reason, "Not Found");
            }
            other => panic!("expected Http error, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("404"), "{msg}");
        assert!(msg.contains("Not Found"), "{msg}");
    }

    #[tokio::test]
    async fn binary_fail_fast() {
        let base = start(router()).await;
        let c = WebClient::new();
        let err = c
            .fetch_docs(&format!("{base}/fetch/binary"))
            .await
            .unwrap_err();
        match &err {
            WebError::Binary {
                content_type,
                byte_length,
                ..
            } => {
                assert_eq!(content_type, "application/octet-stream");
                assert!(*byte_length > 0);
            }
            other => panic!("expected Binary error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn json_passthrough() {
        let base = start(router()).await;
        let c = WebClient::new();
        let r = c.fetch_docs(&format!("{base}/fetch/json")).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        assert!(r.markdown.contains("\"a\""), "got: {}", r.markdown);
        assert!(r.markdown.contains("two"), "got: {}", r.markdown);
    }

    #[tokio::test]
    async fn output_cap_truncates() {
        let base = start(router()).await;
        let c = WebClient::new();
        let r = c.fetch_docs(&format!("{base}/fetch/big")).await.unwrap();
        assert!(
            r.markdown.len() <= MAX_OUTPUT_BYTES + 200,
            "markdown should be capped, got {} bytes",
            r.markdown.len()
        );
        assert!(
            r.markdown.contains("Content truncated"),
            "expected truncation marker, got: {}",
            r.markdown
        );
        assert!(r.length > MAX_OUTPUT_BYTES, "full length should exceed cap");
    }
}
