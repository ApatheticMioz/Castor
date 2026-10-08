//! Web search and document fetching, ported from
//! `mcp-castor/src/harness/services/web_service.js`.
//!
//! `web_search` runs a strict provider chain:
//!   1. SearXNG JSON API (only when `searxng_url` is configured). If it is
//!      configured but unreachable / returns an HTTP error, we surface a
//!      *typed, actionable* error naming what to set or run — we do NOT
//!      silently fall through to another provider.
//!   2. Brave Search API (only when `brave_api_key` is configured).
//!
//! If neither is configured, a typed, actionable `SearxngNotConfiguredError`
//! is returned.
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
        "SearxngUnreachableError: SearXNG at {url} is unreachable ({reason}). Verify the instance is running (e.g. `docker start castor-searxng` or `docker compose up -d`) and that `searxng_url` points at it, or configure a `brave_api_key` to fall back to Brave."
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
    #[error("PdfError: {reason}")]
    PdfParse { reason: String },
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
        }
    }

    /// Build a client with overridden provider base URLs (used by the test
    /// suite to point at local axum mocks).
    pub fn with_base_urls(searxng: Option<String>, brave: Option<String>) -> Self {
        let mut c = Self::new();
        c.searxng_base = searxng;
        if let Some(b) = brave {
            c.brave_base = b;
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
    /// Run the strict provider chain: SearXNG → Brave.
    pub async fn web_search(
        &self,
        query: &str,
        brave_api_key: Option<&str>,
        category: Option<&str>,
    ) -> Result<SearchOutcome, WebError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(WebError::InvalidQuery);
        }

        // 1. SearXNG (only when configured).
        if let Some(base) = self.searxng_base.as_deref() {
            match self.search_searxng(q, base, category).await {
                Ok(results) if !results.is_empty() => {
                    return Ok(SearchOutcome {
                        query: q.to_string(),
                        provider: "searxng".into(),
                        results,
                    });
                }
                Ok(results) => {
                    if let Some(key) = brave_api_key.filter(|k| !k.is_empty()) {
                        let brave_results = self.search_brave(q, key).await?;
                        return Ok(SearchOutcome {
                            query: q.to_string(),
                            provider: "brave".into(),
                            results: brave_results,
                        });
                    }
                    return Ok(SearchOutcome {
                        query: q.to_string(),
                        provider: "searxng".into(),
                        results,
                    });
                }
                Err(e) => {
                    if let Some(key) = brave_api_key.filter(|k| !k.is_empty()) {
                        let brave_results = self.search_brave(q, key).await?;
                        return Ok(SearchOutcome {
                            query: q.to_string(),
                            provider: "brave".into(),
                            results: brave_results,
                        });
                    }
                    return Err(e);
                }
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

        // Neither provider is configured / available.
        Err(WebError::SearxngNotConfigured)
    }

    async fn search_searxng(
        &self,
        q: &str,
        base: &str,
        category: Option<&str>,
    ) -> Result<Vec<SearchResult>, WebError> {
        let base = base.trim_end_matches('/');
        let mut url = reqwest::Url::parse(&format!("{base}/search")).map_err(|e| {
            WebError::SearxngUnreachable {
                url: base.to_string(),
                reason: format!("invalid base url: {e}"),
            }
        })?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("q", q);
            pairs.append_pair("format", "json");
            if let Some(cat) = category.filter(|c| !c.trim().is_empty()) {
                pairs.append_pair("categories", cat.trim());
            }
        }

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
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

impl WebClient {
    /// Fetch a URL and convert it to Markdown (or pass JSON / text / PDF through).
    pub async fn fetch_docs(
        &self,
        url: &str,
        page_range: Option<&str>,
    ) -> Result<FetchResult, WebError> {
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
                "text/html,application/xhtml+xml,application/pdf;q=0.9,application/json;q=0.8,text/plain;q=0.7,*/*;q=0.5",
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
            Category::Pdf => {
                let res = extract_pdf_text(&bytes, page_range)?;
                let len = res.byte_count;
                ("application/pdf".to_string(), res.text, len)
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
    Pdf,
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
        "application/pdf" => return Category::Pdf,
        _ => {}
    }
    if base.ends_with("+json") {
        return Category::Json;
    }
    if base.starts_with("text/") {
        return Category::Text;
    }
    if bytes.starts_with(b"%PDF-") {
        return Category::Pdf;
    }
    if base.starts_with("image/")
        || base.starts_with("audio/")
        || base.starts_with("video/")
        || base == "application/octet-stream"
        || base == "application/zip"
        || base == "application/gzip"
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
        out.push_str("\n```");
    }
    use std::fmt::Write;
    let _ = write!(
        out,
        "\n\n[Content truncated: showing {cut} of {full_len} chars]"
    );
    out
}

// ---------------------------------------------------------------------------
// PDF Text Extraction
// ---------------------------------------------------------------------------

/// The outcome of a PDF text extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfExtractResult {
    /// The extracted text (truncated to `MAX_OUTPUT_BYTES` when necessary).
    pub text: String,
    /// Total page count in the PDF document.
    pub total_pages: usize,
    /// Byte count of the input PDF data.
    pub byte_count: usize,
    /// Whether the output was truncated.
    pub truncated: bool,
    /// The page range actually extracted (1-based, e.g. `"1-3"`).
    pub pages_extracted: String,
}

/// Parse a page-range spec into a 1-based inclusive `(start, end)` pair,
/// clamped to `[1, total]`.
///
/// Accepted forms:
/// - `None` or empty → first 3 pages (or fewer if the doc is shorter).
/// - `"all"` → all pages.
/// - `"N"` → a single page.
/// - `"N-M"` → a range.
///
/// Returns `None` when the range is invalid (e.g. start > end, 0-based).
fn parse_page_range(page_range: Option<&str>, total: usize) -> Option<(usize, usize)> {
    if total == 0 {
        return None;
    }

    let spec = page_range.map(str::trim).filter(|s| !s.is_empty());
    let (start, end) = match spec {
        // No range specified (or empty) → default to first 3 pages.
        None => (1, 3.min(total)),
        // Explicit "all".
        Some("all") => (1, total),
        // "N" or "N-M".
        Some(s) => {
            let parts: Vec<&str> = s.split('-').collect();
            match parts.as_slice() {
                [a] => {
                    let p: usize = a.parse().ok()?;
                    (p, p)
                }
                [a, b] => {
                    let s: usize = a.parse().ok()?;
                    let e: usize = b.parse().ok()?;
                    (s, e)
                }
                _ => return None,
            }
        }
    };

    // Clamp to valid 1-based bounds.
    if start < 1 || end < 1 || start > total || end < start {
        return None;
    }
    let end = end.min(total);
    Some((start, end))
}

/// Extract text from raw PDF bytes, in-memory.
///
/// * Parses the PDF with `lopdf` (pure-Rust, no C bindings).
/// * Supports page-range filtering (`"1-3"`, `"all"`, `"5"`). Defaults to the
///   first 3 pages when no range is given.
/// * Caps the returned text at [`MAX_OUTPUT_BYTES`], appending a structured
///   pagination banner when truncated.
pub fn extract_pdf_text(
    bytes: &[u8],
    page_range: Option<&str>,
) -> Result<PdfExtractResult, WebError> {
    let byte_count = bytes.len();

    let doc = lopdf::Document::load_mem(bytes).map_err(|e| WebError::PdfParse {
        reason: format!("lopdf failed to parse PDF: {e}"),
    })?;

    let pages = doc.get_pages();
    let total_pages = pages.len();

    let (start, end) =
        parse_page_range(page_range, total_pages).ok_or_else(|| WebError::PdfParse {
            reason: format!(
                "invalid page range '{page_range:?}' for a {total_pages}-page document"
            ),
        })?;

    // lopdf's extract_text takes 1-based page numbers as &[u32].
    let page_numbers: Vec<u32> = (start as u32..=end as u32).collect();
    let full_text = doc
        .extract_text(&page_numbers)
        .map_err(|e| WebError::PdfParse {
            reason: format!("text extraction failed: {e}"),
        })?;

    let pages_extracted = if start == end {
        format!("{start}")
    } else {
        format!("{start}-{end}")
    };

    let (text, truncated) = cap_output_pdf(&full_text, start, end, total_pages);

    Ok(PdfExtractResult {
        text,
        total_pages,
        byte_count,
        truncated,
        pages_extracted,
    })
}

/// Truncate PDF text to `MAX_OUTPUT_BYTES`, cutting at a paragraph/line
/// boundary and appending a structured pagination banner.
fn cap_output_pdf(text: &str, start: usize, end: usize, total_pages: usize) -> (String, bool) {
    if text.len() <= MAX_OUTPUT_BYTES {
        return (text.to_string(), false);
    }

    let window = &text[..MAX_OUTPUT_BYTES];
    // Prefer cutting at a double-newline (paragraph) near the cap.
    let search_start = MAX_OUTPUT_BYTES.saturating_sub(500);
    let mut cut = window.rfind("\n\n").unwrap_or(0);
    if cut < search_start {
        cut = window.rfind('\n').unwrap_or(0);
    }
    if cut < search_start {
        cut = MAX_OUTPUT_BYTES;
    }

    let shown = &text[..cut];
    let next_start = end + 1;
    let banner = format!(
        "\n\n[PDF truncated: showing pages {start}-{end} of {total_pages} total. \
         Use page_range=\"{next_start}-{total_pages}\" for the next pages.]"
    );

    (format!("{shown}{banner}"), true)
}

// ---------------------------------------------------------------------------
// Tests (axum mocks on ephemeral ports, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;

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

    async fn searxng_handler(
        axum::extract::Query(params): axum::extract::Query<
            std::collections::HashMap<String, String>,
        >,
    ) -> axum::response::Response {
        if params.get("q").map(|s| s.as_str()) == Some("empty") {
            json_body(r#"{"results":[]}"#)
        } else if let Some(cat) = params.get("categories") {
            json_body(&format!(
                r#"{{"results":[{{"title":"SearXNG {cat} Result","url":"https://searx.example/{cat}","content":"{cat} snippet"}}]}}"#
            ))
        } else {
            json_body(
                r#"{"results":[{"title":"SearXNG Result","url":"https://searx.example/a","content":"sx snippet"}]}"#,
            )
        }
    }

    async fn brave_handler() -> axum::response::Response {
        json_body(
            r#"{"web":{"results":[{"title":"Brave Result","url":"https://brave.example/b","description":"brave snippet"}]}}"#,
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

    async fn fetch_pdf_handler() -> axum::response::Response {
        let bytes = build_minimal_pdf();
        (StatusCode::OK, [("content-type", "application/pdf")], bytes).into_response()
    }

    fn router() -> Router {
        Router::new()
            .route("/searxng/search", get(searxng_handler))
            .route("/brave", get(brave_handler))
            .route("/fetch/html", get(fetch_html_handler))
            .route("/fetch/404", get(fetch_404_handler))
            .route("/fetch/binary", get(fetch_binary_handler))
            .route("/fetch/json", get(fetch_json_handler))
            .route("/fetch/big", get(fetch_big_handler))
            .route("/fetch/pdf", get(fetch_pdf_handler))
    }

    #[tokio::test]
    async fn searxng_results() {
        let base = start(router()).await;
        let c = WebClient::with_base_urls(Some(format!("{base}/searxng")), None);
        let out = c.web_search("rust async", None, None).await.unwrap();
        assert_eq!(out.provider, "searxng");
        assert_eq!(out.query, "rust async");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "SearXNG Result");
        assert_eq!(out.results[0].url, "https://searx.example/a");
        assert_eq!(out.results[0].snippet, "sx snippet");
    }

    #[tokio::test]
    async fn searxng_category_routing() {
        let base = start(router()).await;
        let c = WebClient::with_base_urls(Some(format!("{base}/searxng")), None);
        let out = c
            .web_search("network verification", None, Some("science"))
            .await
            .unwrap();
        assert_eq!(out.provider, "searxng");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "SearXNG science Result");
        assert_eq!(out.results[0].url, "https://searx.example/science");
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
        let c = WebClient::with_base_urls(Some(expected_url.clone()), None);
        let err = c.web_search("rust", None, None).await.unwrap_err();
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
        let c = WebClient::with_base_urls(None, Some(format!("{base}/brave")));
        let out = c.web_search("tokio", Some("bkey"), None).await.unwrap();
        assert_eq!(out.provider, "brave");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "Brave Result");
        assert_eq!(out.results[0].url, "https://brave.example/b");
        assert_eq!(out.results[0].snippet, "brave snippet");
    }

    #[tokio::test]
    async fn searxng_empty_falls_back_to_brave_when_configured() {
        let base = start(router()).await;
        let c = WebClient::with_base_urls(
            Some(format!("{base}/searxng")),
            Some(format!("{base}/brave")),
        );
        let out = c.web_search("empty", Some("bkey"), None).await.unwrap();
        assert_eq!(out.provider, "brave");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "Brave Result");
    }

    #[tokio::test]
    async fn searxng_down_falls_back_to_brave_when_configured() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let base = start(router()).await;
        let c = WebClient::with_base_urls(
            Some(format!("http://127.0.0.1:{port}")),
            Some(format!("{base}/brave")),
        );
        let out = c.web_search("rust", Some("bkey"), None).await.unwrap();
        assert_eq!(out.provider, "brave");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].title, "Brave Result");
    }

    #[tokio::test]
    async fn unconfigured_searxng_yields_typed_actionable_error() {
        let c = WebClient::with_base_urls(None, None);
        let err = c.web_search("serde", None, None).await.unwrap_err();
        match &err {
            WebError::SearxngNotConfigured => {}
            other => panic!("expected SearxngNotConfigured, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("SearxngNotConfiguredError"), "{msg}");
        assert!(msg.contains("searxng_url"), "{msg}");
        assert!(msg.contains("brave_api_key"), "{msg}");
    }

    #[tokio::test]
    async fn html_to_markdown_via_html2md() {
        let base = start(router()).await;
        let c = WebClient::new();
        let r = c
            .fetch_docs(&format!("{base}/fetch/html"), None)
            .await
            .unwrap();
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
            .fetch_docs(&format!("{base}/fetch/404"), None)
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
            .fetch_docs(&format!("{base}/fetch/binary"), None)
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
        let r = c
            .fetch_docs(&format!("{base}/fetch/json"), None)
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        assert!(r.markdown.contains("\"a\""), "got: {}", r.markdown);
        assert!(r.markdown.contains("two"), "got: {}", r.markdown);
    }

    #[tokio::test]
    async fn output_cap_truncates() {
        let base = start(router()).await;
        let c = WebClient::new();
        let r = c
            .fetch_docs(&format!("{base}/fetch/big"), None)
            .await
            .unwrap();
        assert!(
            r.markdown.len() <= MAX_OUTPUT_BYTES + 200,
            "markdown should be capped, got {} bytes",
            r.markdown.len()
        );
        assert!(
            r.markdown.contains("Content truncated"),
            "expected truncation notice, got: {}",
            r.markdown
        );
    }

    #[tokio::test]
    async fn fetch_pdf_basic() {
        let base = start(router()).await;
        let c = WebClient::new();
        let r = c
            .fetch_docs(&format!("{base}/fetch/pdf"), None)
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/pdf");
        assert!(r.markdown.contains("Hello"), "got: {}", r.markdown);
    }

    #[tokio::test]
    async fn fetch_pdf_with_page_range() {
        let base = start(router()).await;
        let c = WebClient::new();
        let r = c
            .fetch_docs(&format!("{base}/fetch/pdf"), Some("1"))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/pdf");
    }

    // -----------------------------------------------------------------------
    // PDF extraction tests
    // -----------------------------------------------------------------------

    /// Build a minimal 1-page PDF containing "Hello" using lopdf's API.
    ///
    /// Uses the raw lopdf object model to create a minimal valid PDF with a
    /// single page whose content stream draws the text "Hello".
    pub fn build_minimal_pdf() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn pdf_extract_basic() {
        let pdf_bytes = build_minimal_pdf();
        let result = extract_pdf_text(&pdf_bytes, None).unwrap();
        assert_eq!(result.total_pages, 1);
        assert_eq!(result.byte_count, pdf_bytes.len());
        assert!(!result.truncated);
        assert_eq!(result.pages_extracted, "1");
        // lopdf's text extraction should recover the literal from the content stream.
        assert!(
            result.text.contains("Hello"),
            "expected 'Hello' in extracted text, got: {:?}",
            result.text
        );
    }

    #[test]
    fn pdf_extract_page_range_single() {
        let pdf_bytes = build_minimal_pdf();
        let result = extract_pdf_text(&pdf_bytes, Some("1")).unwrap();
        assert_eq!(result.total_pages, 1);
        assert_eq!(result.pages_extracted, "1");
        assert!(result.text.contains("Hello"));
    }

    #[test]
    fn pdf_extract_page_range_all() {
        let pdf_bytes = build_minimal_pdf();
        let result = extract_pdf_text(&pdf_bytes, Some("all")).unwrap();
        assert_eq!(result.total_pages, 1);
        assert_eq!(result.pages_extracted, "1");
        assert!(result.text.contains("Hello"));
    }

    #[test]
    fn pdf_extract_out_of_bounds() {
        let pdf_bytes = build_minimal_pdf();
        // Requesting page 5 from a 1-page doc should error.
        let err = extract_pdf_text(&pdf_bytes, Some("5")).unwrap_err();
        match &err {
            WebError::PdfParse { reason } => {
                assert!(reason.contains("invalid page range"), "got: {reason}");
            }
            other => panic!("expected PdfParse error, got {other:?}"),
        }
    }

    #[test]
    fn pdf_extract_invalid_range() {
        let pdf_bytes = build_minimal_pdf();
        // start > end is invalid
        let err = extract_pdf_text(&pdf_bytes, Some("3-1")).unwrap_err();
        assert!(matches!(&err, WebError::PdfParse { .. }));
    }

    #[test]
    fn pdf_extract_garbage_bytes() {
        let garbage = b"this is definitely not a PDF file";
        let err = extract_pdf_text(garbage, None).unwrap_err();
        assert!(matches!(&err, WebError::PdfParse { .. }));
    }

    #[test]
    fn pdf_page_range_parsing() {
        // Default (None) → first 3 pages (clamped).
        assert_eq!(parse_page_range(None, 5), Some((1, 3)));
        // "all"
        assert_eq!(parse_page_range(Some("all"), 10), Some((1, 10)));
        // Single page
        assert_eq!(parse_page_range(Some("3"), 10), Some((3, 3)));
        // Range
        assert_eq!(parse_page_range(Some("2-5"), 10), Some((2, 5)));
        // Range clamped at end
        assert_eq!(parse_page_range(Some("8-20"), 10), Some((8, 10)));
        // Invalid: start > end
        assert_eq!(parse_page_range(Some("5-2"), 10), None);
        // Invalid: zero-based
        assert_eq!(parse_page_range(Some("0"), 10), None);
        // Invalid: start beyond total
        assert_eq!(parse_page_range(Some("11"), 10), None);
        // Single page in 1-page doc
        assert_eq!(parse_page_range(Some("1"), 1), Some((1, 1)));
    }
}
