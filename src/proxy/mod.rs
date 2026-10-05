//! Stream proxy: reverse-proxy to the engine with SSE sanitization.
//!
//! - Listens on `config.ports.proxy` (loopback only).
//! - Forwards `/v1/chat/completions` with `stream:true` to the engine and
//!   sanitizes the SSE response (UTF-8 reassembly, mid-stream error
//!   translation, multimodal guard, repetition circuit breaker).
//! - All other requests are byte-passthrough (including upstream 4xx, which
//!   are forwarded verbatim — never a fake 200 or synthetic finish_reason).
//!
//! Singleton: acquires `Locks::acquire("proxy")` at startup. If the lock is
//! held by another live process, the server exits 0 with a single stderr
//! line. Stale locks are reclaimed via tombstone (see `state::Locks`).

pub mod sanitize;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing;
use futures_util::stream::unfold;
use serde_json::json;

use crate::proxy::sanitize::{SseSanitizer, sanitize_request_body};
use crate::state::{AcquireResult, Locks, StateDir};

/// Inbound request body limit (50 MB, matching the JS proxy).
const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;

/// Singleton lock TTL (30 s).
const LOCK_TTL_MS: u64 = 30_000;

/// The stream proxy, cheaply cloneable (all state behind `Arc`).
#[derive(Clone)]
pub struct ProxyServer {
    inner: Arc<Inner>,
}

struct Inner {
    upstream: SocketAddr,
    locks: Locks,
    client: reqwest::Client,
}

impl ProxyServer {
    pub fn new(state: &StateDir, upstream: SocketAddr) -> Self {
        let locks = Locks::from_state(state);
        let client = reqwest::Client::new();
        Self {
            inner: Arc::new(Inner {
                upstream,
                locks,
                client,
            }),
        }
    }

    /// Acquire the singleton lock. Returns `true` if we own it, `false` if
    /// another live process holds it (caller should exit 0).
    pub fn try_acquire_lock(&self) -> bool {
        match self.inner.locks.acquire("proxy", LOCK_TTL_MS) {
            Ok(AcquireResult::Acquired) => true,
            Ok(AcquireResult::HeldBy { .. }) => false,
            Err(e) => {
                eprintln!("castor: proxy lock acquire error: {e}");
                false
            }
        }
    }

    /// Release the singleton lock (best-effort).
    pub fn release_lock(&self) {
        let _ = self.inner.locks.release("proxy");
    }

    /// Build the axum router.
    fn router(&self) -> Router {
        Router::new()
            .route("/health", routing::get(health_handler))
            .fallback(proxy_handler)
            .with_state(self.clone())
    }

    /// Listen on the given port. Blocks until the server is dropped.
    pub async fn serve(self, port: u16) -> std::io::Result<()> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        eprintln!(
            "[castor-proxy] listening on {addr} -> {}",
            self.inner.upstream
        );
        axum::serve(listener, self.router()).await
    }

    /// Listen on an ephemeral port (for tests). Returns the bound address.
    #[cfg(test)]
    pub async fn serve_ephemeral(self) -> std::io::Result<SocketAddr> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, self.router()).await;
        });
        Ok(addr)
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /health — local health check.
async fn health_handler(State(server): State<ProxyServer>) -> Response {
    let body = json!({
        "status": "ok",
        "service": "castor-proxy",
        "upstream": server.inner.upstream.to_string(),
        "pid": std::process::id(),
    });
    (StatusCode::OK, body.to_string()).into_response()
}

/// Catch-all: forward to the upstream engine.
async fn proxy_handler(
    State(server): State<ProxyServer>,
    req: axum::extract::Request,
) -> Result<Response, StatusCode> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let headers = req.headers().clone();
    let body = req.into_body();

    // Read the inbound body (with a size limit).
    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return Ok((
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({"error":{"message":"Payload Too Large","type":"payload_too_large","code":413}}).to_string(),
            )
                .into_response());
        }
    };

    // Determine if this is a streaming chat request.
    let is_chat = method == axum::http::Method::POST && path.starts_with("/v1/chat/completions");
    let accept_stream = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream"));
    let body_stream = body_bytes
        .windows(15)
        .any(|w| w == b"\"stream\":true" || w == b"\"stream\": true");
    let is_stream = is_chat && (accept_stream || body_stream);

    // Multimodal guard: replace image blocks with text placeholders.
    let mut body_to_send = body_bytes.clone();
    if is_chat
        && let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        && sanitize_request_body(&mut v)
    {
        body_to_send = Bytes::from(v.to_string());
    }

    // Forward to the upstream engine.
    let upstream_url = format!("http://{}{}{}", server.inner.upstream, path, query);
    let mut req_builder = server.inner.client.request(method, &upstream_url);
    for (name, value) in &headers {
        if name.as_str() != "host" {
            req_builder = req_builder.header(name, value);
        }
    }
    let res = match req_builder.body(body_to_send).send().await {
        Ok(r) => r,
        Err(_) => {
            return Ok((
                StatusCode::BAD_GATEWAY,
                json!({"error":{"message":"upstream connection error","type":"upstream_connection_error"}}).to_string(),
            )
                .into_response());
        }
    };

    let status = res.status();
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_event_stream = content_type.contains("text/event-stream") || is_stream;

    // Build the response, forwarding the upstream status and headers.
    let mut resp_builder = Response::builder().status(status);
    for (name, value) in res.headers() {
        if name.as_str() != "content-length" && name.as_str() != "transfer-encoding" {
            resp_builder = resp_builder.header(name, value);
        }
    }

    if is_event_stream && status.is_success() {
        // Streaming SSE: sanitize the response.
        resp_builder = resp_builder
            .header("content-type", "text/event-stream; charset=utf-8")
            .header("cache-control", "no-cache, no-transform");
        let stream = sse_stream(res);
        Ok(resp_builder.body(Body::from_stream(stream)).unwrap())
    } else {
        // Byte passthrough (including upstream 4xx — verbatim, never faked).
        let stream = passthrough_stream(res);
        Ok(resp_builder.body(Body::from_stream(stream)).unwrap())
    }
}

// ---------------------------------------------------------------------------
// Stream builders
// ---------------------------------------------------------------------------

struct SseState {
    res: reqwest::Response,
    sanitizer: SseSanitizer,
    finished: bool,
}

/// Stream the upstream SSE response through the sanitizer.
fn sse_stream(
    res: reqwest::Response,
) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> {
    let state = SseState {
        res,
        sanitizer: SseSanitizer::new(),
        finished: false,
    };
    unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        match state.res.chunk().await {
            Ok(Some(bytes)) => {
                let out = state.sanitizer.feed(&bytes);
                Some((Ok(Bytes::from(out)), state))
            }
            Ok(None) => {
                let out = state.sanitizer.finish();
                state.finished = true;
                Some((Ok(Bytes::from(out)), state))
            }
            Err(_) => {
                let out = "event: error\ndata: {\"error\":{\"message\":\"upstream error\",\"type\":\"upstream_error\"}}\n\n";
                state.finished = true;
                Some((Ok(Bytes::from(out)), state))
            }
        }
    })
}

struct PassthroughState {
    res: reqwest::Response,
    finished: bool,
}

/// Stream the upstream response body verbatim (byte passthrough).
fn passthrough_stream(
    res: reqwest::Response,
) -> impl futures_util::Stream<Item = Result<Bytes, Infallible>> {
    let state = PassthroughState {
        res,
        finished: false,
    };
    unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        match state.res.chunk().await {
            Ok(Some(bytes)) => Some((Ok(bytes), state)),
            Ok(None) | Err(_) => None,
        }
    })
}

// ---------------------------------------------------------------------------
// Spawn helper (used by the `mcp` subcommand)
// ---------------------------------------------------------------------------

/// Best-effort, non-blocking bring-up of the proxy, invoked by the `mcp`
/// subcommand before it starts serving.
///
/// The sequence is **lock → probe → spawn if down → release** (mirroring
/// `ensure_status_server`). Every step is best-effort: a failure is logged
/// to stderr and swallowed. This never blocks the MCP server from starting.
pub async fn ensure_proxy_server(state: &StateDir, proxy_port: u16, engine_port: u16) {
    let upstream = SocketAddr::from(([127, 0, 0, 1], engine_port));
    let server = ProxyServer::new(state, upstream);

    // 1. lock.
    let acquired = server.try_acquire_lock();

    // 2. probe: is a proxy already listening on the port?
    let already_up = probe_port(proxy_port).await;

    if acquired && !already_up {
        // 3. spawn (detached child re-acquires the lock).
        spawn_detached(state, proxy_port, engine_port);
        // 4. release so the child can take the lock.
        server.release_lock();
    } else {
        if acquired {
            server.release_lock();
        }
        eprintln!("[castor] proxy already up (port {proxy_port}); not spawning");
    }
}

/// Probe the proxy port with a short-timeout TCP connect.
async fn probe_port(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let res = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(addr),
    )
    .await;
    matches!(res, Ok(Ok(_)))
}

/// Spawn the proxy as a fully detached child process (new process group on
/// Unix, stdio nulled). Never waits for the child.
fn spawn_detached(state: &StateDir, proxy_port: u16, engine_port: u16) {
    let Some(exe) = std::env::current_exe().ok() else {
        eprintln!("[castor] cannot determine current exe; skipping proxy spawn");
        return;
    };
    let state_str = state.root().to_string_lossy().into_owned();
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("proxy")
        .arg("--state-dir")
        .arg(&state_str)
        .arg("--port")
        .arg(proxy_port.to_string())
        .arg("--engine-port")
        .arg(engine_port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    match cmd.spawn() {
        Ok(_) => {
            eprintln!("[castor] proxy spawned (port {proxy_port})");
        }
        Err(e) => {
            eprintln!("[castor] failed to spawn proxy: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering as Ord};

    static TMP: AtomicUsize = AtomicUsize::new(0);

    fn tmp_state() -> StateDir {
        let n = TMP.fetch_add(1, Ord::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-proxy-{}-{}", std::process::id(), n));
        let s = StateDir::new(p);
        s.ensure().unwrap();
        s
    }

    /// Start a mock upstream engine on an ephemeral port.
    async fn start_mock_upstream() -> SocketAddr {
        let app = Router::new().route("/v1/chat/completions", routing::post(mock_upstream_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    /// Mock upstream: serves different test vectors based on the `vector`
    /// query parameter.
    async fn mock_upstream_handler(Query(q): Query<HashMap<String, String>>) -> Response {
        let vector = q.get("vector").cloned().unwrap_or_default();
        match vector.as_str() {
            "4xx" => (
                StatusCode::BAD_REQUEST,
                json!({"error":{"message":"bad request","type":"invalid_request"}}).to_string(),
            )
                .into_response(),
            "error" => {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: {\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}\n\n";
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            }
            "emoji" => {
                let full = "data: {\"choices\":[{\"delta\":{\"content\":\"😀\"}}]}\n\n";
                let bytes = full.as_bytes().to_vec();
                let emoji_bytes: &[u8] = &[0xF0, 0x9F, 0x98, 0x80];
                let pos = bytes
                    .windows(4)
                    .position(|w| w == emoji_bytes)
                    .expect("emoji not found");
                let split_at = pos + 2;
                let chunk1 = Bytes::from(bytes[..split_at].to_vec());
                let chunk2 = Bytes::from(bytes[split_at..].to_vec());
                let stream =
                    futures_util::stream::iter(vec![Ok::<_, Infallible>(chunk1), Ok(chunk2)]);
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
            "invalid" => {
                let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\xFFb\"}}]}\n\n";
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(body.to_vec()))
                    .unwrap()
            }
            "repetition" => {
                let content = "a".repeat(100);
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                    content
                );
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            }
            _ => (
                StatusCode::NOT_FOUND,
                json!({"error":{"message":"unknown vector"}}).to_string(),
            )
                .into_response(),
        }
    }

    /// Send a streaming chat request to the proxy and return (status, body).
    async fn proxy_stream_request(proxy_addr: &SocketAddr, vector: &str) -> (u16, String) {
        let url = format!("http://{proxy_addr}/v1/chat/completions?vector={vector}");
        let client = reqwest::Client::new();
        let res = client
            .post(&url)
            .header("accept", "text/event-stream")
            .body(
                json!({"model":"test","stream":true,"messages":[{"role":"user","content":"hi"}]})
                    .to_string(),
            )
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        let body = res.text().await.unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn proxy_4xx_passthrough_verbatim() {
        let upstream = start_mock_upstream().await;
        let state = tmp_state();
        let proxy = ProxyServer::new(&state, upstream);
        let proxy_addr = proxy.serve_ephemeral().await.unwrap();

        let (status, body) = proxy_stream_request(&proxy_addr, "4xx").await;
        assert_eq!(status, 400, "4xx must be passed through verbatim");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["message"], "bad request");
        assert_eq!(v["error"]["type"], "invalid_request");
    }

    #[tokio::test]
    async fn proxy_mid_stream_error_no_done() {
        let upstream = start_mock_upstream().await;
        let state = tmp_state();
        let proxy = ProxyServer::new(&state, upstream);
        let proxy_addr = proxy.serve_ephemeral().await.unwrap();

        let (status, body) = proxy_stream_request(&proxy_addr, "error").await;
        assert_eq!(status, 200);
        assert!(body.contains("event: error"), "got: {body}");
        assert!(body.contains("boom"), "got: {body}");
        assert!(
            !body.contains("[DONE]"),
            "error frame must NOT append [DONE]: {body}"
        );
    }

    #[tokio::test]
    async fn proxy_emoji_intact() {
        let upstream = start_mock_upstream().await;
        let state = tmp_state();
        let proxy = ProxyServer::new(&state, upstream);
        let proxy_addr = proxy.serve_ephemeral().await.unwrap();

        let (status, body) = proxy_stream_request(&proxy_addr, "emoji").await;
        assert_eq!(status, 200);
        assert!(
            body.contains("\u{1F600}"),
            "emoji must arrive intact, got: {body}"
        );
        assert!(!body.contains('\u{FFFD}'), "no replacement chars: {body}");
    }

    #[tokio::test]
    async fn proxy_invalid_byte_fffd() {
        let upstream = start_mock_upstream().await;
        let state = tmp_state();
        let proxy = ProxyServer::new(&state, upstream);
        let proxy_addr = proxy.serve_ephemeral().await.unwrap();

        let (status, body) = proxy_stream_request(&proxy_addr, "invalid").await;
        assert_eq!(status, 200);
        assert!(
            body.contains("\u{FFFD}"),
            "invalid byte must become U+FFFD, got: {body}"
        );
    }

    #[tokio::test]
    async fn proxy_repetition_breaker() {
        let upstream = start_mock_upstream().await;
        let state = tmp_state();
        let proxy = ProxyServer::new(&state, upstream);
        let proxy_addr = proxy.serve_ephemeral().await.unwrap();

        let (status, body) = proxy_stream_request(&proxy_addr, "repetition").await;
        assert_eq!(status, 200);
        assert!(
            body.contains(crate::proxy::sanitize::GUARD_MARKER_PREFIX),
            "got: {body}"
        );
        assert!(body.contains("finish_reason"), "got: {body}");
        assert!(
            body.contains("[DONE]"),
            "breaker must append [DONE]: {body}"
        );
    }

    #[tokio::test]
    async fn proxy_lock_held_second_instance() {
        let state = tmp_state();
        let upstream = SocketAddr::from(([127, 0, 0, 1], 18020));
        let proxy = ProxyServer::new(&state, upstream);
        // First acquire succeeds.
        assert!(proxy.try_acquire_lock());
        // Second server (same state dir) should find the lock held.
        let proxy2 = ProxyServer::new(&state, upstream);
        assert!(
            !proxy2.try_acquire_lock(),
            "second server must see lock held"
        );
        // Release; now the second can acquire.
        proxy.release_lock();
        assert!(proxy2.try_acquire_lock());
        proxy2.release_lock();
    }
}
