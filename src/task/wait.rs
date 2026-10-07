//! Status / long-poll HTTP server.
//!
//! Faithful port of the `mcp-castor` status-server semantics
//! (`task_registry.js` `statusHttpServer` + `wait_endpoint.test.js` +
//! `status_lifecycle.test.js`):
//!
//! - **Singleton**: acquires `Locks::acquire("status")` at startup. If the
//!   lock is held by another live process, the server exits 0 with a single
//!   stderr line `"status server already running"`.
//! - **GET /task/:id/wait?timeout_s=N** — long-poll. Wakes on a terminal
//!   transition (per-task broadcast channel) or a disk-mirror poll (500 ms)
//!   for tasks owned by other instances. Terminal ⇒ ALWAYS HTTP 200 JSON
//!   (success OR failure — never masked). Timeout ⇒ 200 `{status:"executing",
//!   timed_out:true}`. Unknown id ⇒ 404.
//! - **GET /task/:id/status** — computed `elapsed_s` (queued AND executing).
//!   Malformed disk file ⇒ 400 `{error}` (never 500).
//!
//! The `mcp` subcommand spawns this as a detached child (lock → probe →
//! spawn if down → release), best-effort, never blocking the MCP server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::json;

use crate::state::{AcquireResult, Locks, StateDir};
use crate::task::registry::{DiskRead, TaskRecord, TaskRegistry, TaskStatus};

/// Disk-mirror poll interval for cross-instance waiters (500 ms).
const DISK_POLL_MS: u64 = 500;

/// The status server, cheaply cloneable (all state behind `Arc`).
#[derive(Clone)]
pub struct StatusServer {
    inner: Arc<Inner>,
}

struct Inner {
    registry: TaskRegistry,
    locks: Locks,
}

impl StatusServer {
    pub fn new(state: &StateDir) -> Self {
        let registry = TaskRegistry::new(state);
        let locks = Locks::from_state(state);
        Self {
            inner: Arc::new(Inner { registry, locks }),
        }
    }

    /// Acquire the singleton lock. Returns `true` if we own it, `false` if
    /// another live process holds it (caller should exit 0).
    pub fn try_acquire_lock(&self) -> bool {
        let ttl = 30_000; // 30 s TTL; the server renews implicitly by holding.
        match self.inner.locks.acquire("status", ttl) {
            Ok(AcquireResult::Acquired) => true,
            Ok(AcquireResult::HeldBy { .. }) => false,
            Err(e) => {
                eprintln!("castor: status lock acquire error: {e}");
                false
            }
        }
    }

    /// Release the singleton lock (best-effort).
    pub fn release_lock(&self) {
        let _ = self.inner.locks.release("status");
    }

    /// Build the axum router.
    fn router(&self) -> Router {
        Router::new()
            .route("/task/{id}/wait", get(wait_handler))
            .route("/task/{id}/status", get(status_handler))
            .with_state(self.clone())
    }

    /// Listen on the given port. Blocks until the server is dropped or the
    /// process is signalled.
    pub async fn serve(self, port: u16) -> std::io::Result<()> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        eprintln!("[castor-status] listening on {addr}");
        axum::serve(listener, self.router()).await
    }

    /// Listen on an ephemeral port (for tests). Returns the bound address.
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
// Query params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WaitQuery {
    #[serde(default = "default_timeout")]
    timeout_s: u64,
}

fn default_timeout() -> u64 {
    3600
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /task/:id/wait?timeout_s=N
///
/// Long-poll: waits until the task reaches a terminal state or the timeout
/// elapses. Terminal ⇒ 200 JSON (always, even on failure). Timeout ⇒ 200
/// `{status:"executing", timed_out:true}`. Unknown id ⇒ 404.
async fn wait_handler(
    State(server): State<StatusServer>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<WaitQuery>,
) -> Response {
    let reg = &server.inner.registry;

    // Orphan reaper before reads: a dead-pid executing task is marked failed
    // (idempotent) so the wait resolves with an honest terminal state.
    let _ = reg.reap_orphans().await;

    // Fast path: already terminal (memory or disk).
    if let Some(rec) = reg.get(&id).await {
        if rec.status.is_terminal() {
            return terminal_response(&rec).into_response();
        }
    } else {
        // Not in memory: check disk.
        match reg.read_disk(&id) {
            DiskRead::Ok(rec) if rec.status.is_terminal() => {
                return terminal_response(&rec).into_response();
            }
            DiskRead::Ok(_) => {}
            DiskRead::NotFound => {
                return (
                    StatusCode::NOT_FOUND,
                    json!({"error": "task not found"}).to_string(),
                )
                    .into_response();
            }
            DiskRead::Corrupt(msg) => {
                return (
                    StatusCode::BAD_REQUEST,
                    json!({"error": format!("malformed task file: {msg}")}).to_string(),
                )
                    .into_response();
            }
        }
    }

    // Not terminal yet: subscribe and wait.
    let rx = reg.subscribe_terminal(&id).await;
    let timeout = Duration::from_secs(q.timeout_s);

    tokio::select! {
        _ = wait_for_terminal(rx, reg, &id, timeout) => {}
    };

    // Re-read after the wait: the task may have gone terminal.
    if let Some(rec) = reg.get(&id).await
        && rec.status.is_terminal()
    {
        return terminal_response(&rec).into_response();
    }
    // Also check disk (cross-instance).
    if let DiskRead::Ok(rec) = reg.read_disk(&id)
        && rec.status.is_terminal()
    {
        return terminal_response(&rec).into_response();
    }

    // Timed out.
    (
        StatusCode::OK,
        json!({"status": "executing", "timed_out": true}).to_string(),
    )
        .into_response()
}

/// Wait for a terminal transition on a task, polling the disk mirror for
/// cross-instance tasks. Returns when the task is terminal or the timeout
/// elapses.
async fn wait_for_terminal(
    mut rx: tokio::sync::broadcast::Receiver<u64>,
    reg: &TaskRegistry,
    id: &str,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        // Wait for either a broadcast wake or the poll interval.
        let poll = Duration::from_millis(DISK_POLL_MS);
        let wait_dur = remaining.min(poll);
        tokio::select! {
            _ = rx.recv() => {
                // Woken by a terminal transition; check state.
                if let Some(rec) = reg.get(id).await
                    && rec.status.is_terminal() {
                        return;
                    }
            }
            _ = tokio::time::sleep(wait_dur) => {
                // Poll the disk mirror (cross-instance fallback).
                if let DiskRead::Ok(rec) = reg.read_disk(id)
                    && rec.status.is_terminal() {
                        return;
                    }
            }
        }
    }
}

/// GET /task/:id/status
///
/// Returns the task's current status with a computed `elapsed_s`.
/// Malformed disk file ⇒ 400 `{error}` (never 500). Unknown id ⇒ 404.
async fn status_handler(
    State(server): State<StatusServer>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let reg = &server.inner.registry;

    // Orphan reaper before reads.
    let _ = reg.reap_orphans().await;

    // Memory first.
    if let Some(rec) = reg.get(&id).await {
        return status_response(&rec).into_response();
    }

    // Disk fallback.
    match reg.read_disk(&id) {
        DiskRead::Ok(rec) => status_response(&rec).into_response(),
        DiskRead::NotFound => (
            StatusCode::NOT_FOUND,
            json!({"error": "task not found"}).to_string(),
        )
            .into_response(),
        DiskRead::Corrupt(msg) => (
            StatusCode::BAD_REQUEST,
            json!({"error": format!("malformed task file: {msg}")}).to_string(),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

/// Build the terminal-response JSON (always 200, never masked).
fn terminal_response(rec: &TaskRecord) -> (StatusCode, String) {
    let now = now_ms();
    let elapsed_s = elapsed_seconds(rec, now);
    let body = json!({
        "found": true,
        "id": rec.id,
        "status": rec.status,
        "reason": rec.reason,
        "session_id": rec.session_id,
        "cwd": rec.cwd,
        "elapsed_s": elapsed_s,
        "created_at": rec.created_at,
        "started_at": rec.started_at,
        "ended_at": rec.ended_at,
        "turns_budget": rec.turns_budget,
    });
    (StatusCode::OK, body.to_string())
}

/// Build the status-response JSON with computed `elapsed_s`.
fn status_response(rec: &TaskRecord) -> (StatusCode, String) {
    let now = now_ms();
    let elapsed_s = elapsed_seconds(rec, now);
    let body = json!({
        "found": true,
        "id": rec.id,
        "status": rec.status,
        "reason": rec.reason,
        "session_id": rec.session_id,
        "cwd": rec.cwd,
        "elapsed_s": elapsed_s,
        "created_at": rec.created_at,
        "started_at": rec.started_at,
        "ended_at": rec.ended_at,
        "turns_budget": rec.turns_budget,
    });
    (StatusCode::OK, body.to_string())
}

/// Compute elapsed seconds from the task's creation (or start) to now.
///
/// - `queued`: elapsed since `created_at`.
/// - `executing`: elapsed since `started_at` (falling back to `created_at`).
/// - terminal: elapsed since `created_at` to `ended_at`.
fn elapsed_seconds(rec: &TaskRecord, now: u64) -> u64 {
    let end = rec.ended_at.unwrap_or(now);
    let start = match rec.status {
        TaskStatus::Executing => rec.started_at.unwrap_or(rec.created_at),
        _ => rec.created_at,
    };
    end.saturating_sub(start) / 1000
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Spawn helper (used by the `mcp` subcommand)
// ---------------------------------------------------------------------------

/// Best-effort, non-blocking bring-up of the status server, invoked by the
/// `mcp` subcommand before it starts serving.
///
/// The sequence is **lock → probe → spawn if down → release**:
///
/// 1. **lock** — try to acquire the `status` singleton lock. If another live
///    process already holds it, a status server is running; we do not spawn.
/// 2. **probe** — check whether something is already listening on the status
///    port (a live keeper may hold a stale lock, or a foreign process may own
///    the port). If the port is already serving, we do not spawn.
/// 3. **spawn** — only when we own the lock *and* the port is dark do we spawn
///    a detached `castor status` child.
/// 4. **release** — release the lock we acquired so the spawned child can
///    acquire it itself.
///
/// Every step is best-effort: a failure is logged to stderr and swallowed.
/// This never blocks the MCP server from starting.
pub async fn ensure_status_server(state: &StateDir, port: u16) {
    let server = StatusServer::new(state);

    // 1. lock.
    let acquired = server.try_acquire_lock();

    // 2. probe: is a status server already listening on the port?
    let already_up = probe_status_port(port).await;

    if acquired && !already_up {
        // 3. spawn (detached child re-acquires the lock).
        spawn_detached(state, port);
        // 4. release so the child can take the lock.
        server.release_lock();
    } else {
        // We either don't own the lock (another live process does) or the
        // port is already serving. Release our lock if we hold it; do not
        // spawn.
        if acquired {
            server.release_lock();
        }
        eprintln!("[castor] status server already up (port {port}); not spawning");
    }
}

/// Probe the status port with a short-timeout TCP connect. Returns `true` if
/// something is listening (a live keeper), `false` if the port is dark.
async fn probe_status_port(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let res = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(addr),
    )
    .await;
    matches!(res, Ok(Ok(_)))
}

/// Spawn the status server as a fully detached child process (new process
/// group on Unix, stdio nulled). Never waits for the child.
fn spawn_detached(state: &StateDir, port: u16) {
    let Some(exe) = std::env::current_exe().ok() else {
        eprintln!("[castor] cannot determine current exe; skipping status server spawn");
        return;
    };
    let state_str = state.root().to_string_lossy().into_owned();
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("status")
        .arg("--state-dir")
        .arg(&state_str)
        .arg("--port")
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach: put the child in its own process group (pgid 0 = the child's
    // own pid) so it survives the parent and is not reaped with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    match cmd.spawn() {
        Ok(_) => {
            eprintln!("[castor] status server spawned (port {port})");
        }
        Err(e) => {
            eprintln!("[castor] failed to spawn status server: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as Ord};

    static TMP: AtomicUsize = AtomicUsize::new(0);

    fn tmp_state() -> StateDir {
        let n = TMP.fetch_add(1, Ord::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-wait-{}-{}", std::process::id(), n));
        let s = StateDir::new(p);
        s.ensure().unwrap();
        s
    }

    /// A dead pid (spawn a child, wait for it to exit).
    fn dead_pid() -> u32 {
        #[cfg(windows)]
        let mut c = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        #[cfg(not(windows))]
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        let _ = c.wait();
        pid
    }

    /// Make an HTTP GET and return (status_code, body).
    async fn http_get(addr: &SocketAddr, path: &str) -> (u16, String) {
        let url = format!("http://{addr}{path}");
        let client = reqwest::Client::new();
        let res = client.get(&url).send().await.unwrap();
        let status = res.status().as_u16();
        let body = res.text().await.unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn wait_wakes_on_completed() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        let reg = server.inner.registry.clone();
        let addr = server.serve_ephemeral().await.unwrap();

        let id = reg.create("do it", "/tmp", "s1").await.unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();

        // Spawn a wait with a 5s timeout; transition to completed after 100ms.
        let reg2 = reg.clone();
        let id2 = id.clone();
        let handle = tokio::spawn(async move {
            let url = format!("http://{addr}/task/{id2}/wait?timeout_s=5");
            let client = reqwest::Client::new();
            let res = client.get(&url).send().await.unwrap();
            (res.status().as_u16(), res.text().await.unwrap())
        });

        // Give the wait a moment to subscribe, then transition.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = reg2
            .transition(&id, TaskStatus::Completed, None)
            .await
            .unwrap();

        let (status, body) = handle.await.unwrap();
        assert_eq!(status, 200, "wait should return 200 on terminal");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "completed");
        assert_eq!(v["found"], true);
        assert!(
            !v.get("timed_out")
                .and_then(|t| t.as_bool())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn wait_wakes_on_failed_with_honest_status() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        let reg = server.inner.registry.clone();
        let addr = server.serve_ephemeral().await.unwrap();

        let id = reg.create("do it", "/tmp", "s2").await.unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();

        let reg2 = reg.clone();
        let id2 = id.clone();
        let handle = tokio::spawn(async move {
            let url = format!("http://{addr}/task/{id2}/wait?timeout_s=5");
            let client = reqwest::Client::new();
            let res = client.get(&url).send().await.unwrap();
            (res.status().as_u16(), res.text().await.unwrap())
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = reg2
            .transition(&id, TaskStatus::Failed, Some("engine error".into()))
            .await
            .unwrap();

        let (status, body) = handle.await.unwrap();
        assert_eq!(status, 200, "failure must be 200, not 500");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["reason"], "engine error");
    }

    #[tokio::test]
    async fn wait_timeout_path() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        let reg = server.inner.registry.clone();
        let addr = server.serve_ephemeral().await.unwrap();

        let id = reg.create("do it", "/tmp", "s3").await.unwrap();
        // Leave it executing; no transition.

        let (status, body) = http_get(&addr, &format!("/task/{id}/wait?timeout_s=1")).await;
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "executing");
        assert_eq!(v["timed_out"], true);
    }

    #[tokio::test]
    async fn wait_404_unknown_id() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        let addr = server.serve_ephemeral().await.unwrap();

        let (status, body) = http_get(&addr, "/task/nonexistent_xyz/wait").await;
        assert_eq!(status, 404);
        assert!(
            body.to_lowercase().contains("not found"),
            "body should mention not found: {body}"
        );
    }

    #[tokio::test]
    async fn status_malformed_file_returns_400() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        let addr = server.serve_ephemeral().await.unwrap();

        // Write a corrupt task file.
        let path = state.tasks().join("task_corrupt.json");
        std::fs::write(&path, "{ not valid json !").unwrap();

        let (status, body) = http_get(&addr, "/task/task_corrupt/status").await;
        assert_eq!(status, 400, "malformed file must be 400, not 500");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("malformed"),
            "error should name the problem: {body}"
        );
    }

    #[tokio::test]
    async fn status_elapsed_queued_and_executing() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        let reg = server.inner.registry.clone();
        let addr = server.serve_ephemeral().await.unwrap();

        // Queued task: created 60s ago.
        let id_q = reg.create("q", "/tmp", "sq").await.unwrap();
        let _ = reg
            .update(&id_q, |r| {
                r.created_at = now_ms() - 60_000;
            })
            .await
            .unwrap();
        let (status, body) = http_get(&addr, &format!("/task/{id_q}/status")).await;
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "queued");
        let elapsed = v["elapsed_s"].as_u64().unwrap();
        assert!(
            (55..=65).contains(&elapsed),
            "queued elapsed should be ~60s, got {elapsed}"
        );

        // Executing task: started 30s ago.
        let id_e = reg.create("e", "/tmp", "se").await.unwrap();
        let _ = reg
            .transition(&id_e, TaskStatus::Executing, None)
            .await
            .unwrap();
        let _ = reg
            .update(&id_e, |r| {
                r.created_at = now_ms() - 90_000;
                r.started_at = Some(now_ms() - 30_000);
            })
            .await
            .unwrap();
        let (status, body) = http_get(&addr, &format!("/task/{id_e}/status")).await;
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "executing");
        let elapsed = v["elapsed_s"].as_u64().unwrap();
        assert!(
            (25..=35).contains(&elapsed),
            "executing elapsed should be ~30s, got {elapsed}"
        );
    }

    #[tokio::test]
    async fn orphan_reaper_before_reads() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        let reg = server.inner.registry.clone();
        let addr = server.serve_ephemeral().await.unwrap();

        // Create an executing task with a dead pid.
        let id = reg.create("orphan", "/tmp", "so").await.unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();
        let dead = dead_pid();
        let _ = reg.update(&id, |r| r.pid = Some(dead)).await.unwrap();

        // Wait should resolve with failed (orphan reaper runs before the read).
        let (status, body) = http_get(&addr, &format!("/task/{id}/wait?timeout_s=3")).await;
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["reason"], "orphaned worker died");

        // Idempotent: a second wait also returns failed (not a double-transition).
        let (status2, body2) = http_get(&addr, &format!("/task/{id}/wait?timeout_s=1")).await;
        assert_eq!(status2, 200);
        let v2: serde_json::Value = serde_json::from_str(&body2).unwrap();
        assert_eq!(v2["status"], "failed");
    }

    #[tokio::test]
    async fn singleton_lock_held_exits_cleanly() {
        let state = tmp_state();
        let server = StatusServer::new(&state);
        // First acquire succeeds.
        assert!(server.try_acquire_lock());
        // Second server (same state dir) should find the lock held.
        let server2 = StatusServer::new(&state);
        assert!(
            !server2.try_acquire_lock(),
            "second server must see lock held"
        );
        // Release; now the second can acquire.
        server.release_lock();
        assert!(server2.try_acquire_lock());
        server2.release_lock();
    }
}
