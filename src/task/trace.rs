//! Castor Trace visualizer: native Axum web server and API routes.
//!
//! Provides an embedded, zero-dependency visualizer workbench for Castor task
//! executions and session event streams. Embedded at compile-time via `include_str!`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::StateDir;
use crate::task::semaphore::pid_alive;

/// Embedded single-file workbench frontend (compiled directly into the binary).
pub const TRACE_HTML: &str = include_str!("trace.html");

// ---------------------------------------------------------------------------
// Wire Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub prompt: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(rename = "endedAt", skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    pub mtime: u64,
    #[serde(rename = "turnsBudget")]
    pub turns_budget: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TasksResponse {
    pub tasks: Vec<TaskSummary>,
    pub total: usize,
}

#[derive(Debug, Deserialize)]
pub struct TasksQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct PollQuery {
    #[serde(alias = "taskId")]
    pub task_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PollResponse {
    #[serde(rename = "activeTaskId")]
    pub active_task_id: Option<String>,
    pub status: String,
    pub mtime: u64,
    #[serde(rename = "latestTaskId")]
    pub latest_task_id: Option<String>,
    #[serde(rename = "latestStatus")]
    pub latest_status: String,
    #[serde(rename = "totalTasks")]
    pub total_tasks: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnGroup {
    pub turn: u32,
    pub dispatch: Option<Value>,
    pub results: Vec<Value>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TaskTraceResponse {
    pub task: Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(rename = "totalEvents")]
    pub total_events: usize,
    #[serde(rename = "turnsCount")]
    pub turns_count: usize,
    pub turns: Vec<TurnGroup>,
}

#[derive(Debug, Serialize)]
pub struct ContextConfigResponse {
    #[serde(rename = "maxContext")]
    pub max_context: Option<u32>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    headers
}

/// Display-layer orphan reconciliation for the read-only visualizer.
///
/// Returns true when a task record is a *ghost*: it is marked `executing`
/// but its worker pid is dead (crashed session, killed worker). The UI must
/// never show such a record as "LIVE", so both `read_recent_tasks` and
/// `poll_handler` downgrade its displayed status to `orphaned`. This is
/// purely a display projection of the on-disk record; the durable fix (rewriting
/// the record to a terminal `failed` state) is the boot-time reaper in
/// [`crate::task::registry`], which converges the display within one poll.
///
/// `executing` *without* a pid is the in-flight race between the registry
/// write and the worker spawn, so it is treated as alive to avoid flapping a
/// healthy task to `orphaned` for a poll cycle.
fn is_orphaned(status: &str, pid: Option<u32>) -> bool {
    if status != "executing" {
        return false;
    }
    matches!(pid, Some(p) if !pid_alive(p))
}

/// Scan `<state>/tasks` for task JSON records, sorted newest first by mtime.
/// Returns a tuple of (summaries, total_count).
pub fn read_recent_tasks(state: &StateDir, limit: usize) -> (Vec<TaskSummary>, usize) {
    let dir = state.tasks();
    let Ok(entries) = fs::read_dir(&dir) else {
        return (Vec::new(), 0);
    };

    let mut files_with_mtime: Vec<(PathBuf, u64)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        if file_name.starts_with(".tmp")
            || file_name.contains(".tmp_")
            || file_name.starts_with("job_")
        {
            continue;
        }

        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map_err(std::io::Error::other)
            })
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        files_with_mtime.push((path, mtime));
    }

    files_with_mtime.sort_by_key(|a| std::cmp::Reverse(a.1));
    let total_count = files_with_mtime.len();

    let mut summaries = Vec::new();
    for (path, mtime) in files_with_mtime.into_iter().take(limit) {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };

        let file_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let id = val
            .get("id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or(file_stem);

        let pid = val.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32);

        // Honest display: an `executing` task whose worker pid is dead is an
        // orphan (reboot / crash). Show it as `orphaned` rather than a ghost
        // "LIVE"; the boot-time reaper converges it to a terminal `failed`.
        let status = {
            let raw = val
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            if is_orphaned(&raw, pid) {
                "orphaned".to_string()
            } else {
                raw
            }
        };

        let session_id = val
            .get("session_id")
            .or_else(|| val.get("sessionId"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let cwd = val.get("cwd").and_then(|v| v.as_str()).map(String::from);
        let prompt = val
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let created_at = val
            .get("created_at")
            .and_then(|v| v.as_u64())
            .unwrap_or(mtime);

        let started_at = val.get("started_at").and_then(|v| v.as_u64());
        let ended_at = val.get("ended_at").and_then(|v| v.as_u64());
        let turns_budget = val
            .get("turns_budget")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        summaries.push(TaskSummary {
            id,
            status,
            session_id,
            cwd,
            prompt,
            created_at,
            started_at,
            ended_at,
            mtime,
            turns_budget,
            pid,
        });
    }

    (summaries, total_count)
}

/// Find a specific task file in `<state>/tasks`.
fn find_task_file(state: &StateDir, id: &str) -> Option<PathBuf> {
    let direct = state.tasks().join(format!("{id}.json"));
    if direct.is_file() {
        return Some(direct);
    }

    let Ok(entries) = fs::read_dir(state.tasks()) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem == id || stem.contains(id) {
                return Some(path);
            }
        }
    }
    None
}

/// Read a task's full execution trace (task record + grouped turns).
pub fn parse_task_trace(state: &StateDir, task_id: &str) -> Option<TaskTraceResponse> {
    let task_path = find_task_file(state, task_id)?;
    let raw_task = fs::read_to_string(&task_path).ok()?;
    let task_val: Value = serde_json::from_str(&raw_task).ok()?;

    let session_id = task_val
        .get("session_id")
        .or_else(|| task_val.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let Some(ref sess_id) = session_id else {
        return Some(TaskTraceResponse {
            task: task_val,
            session_id: None,
            total_events: 0,
            turns_count: 0,
            turns: Vec::new(),
        });
    };

    let events_path = state.sessions().join(sess_id).join("events.jsonl");
    if !events_path.is_file() {
        return Some(TaskTraceResponse {
            task: task_val,
            session_id: Some(sess_id.clone()),
            total_events: 0,
            turns_count: 0,
            turns: Vec::new(),
        });
    }

    let Ok(file) = File::open(&events_path) else {
        return Some(TaskTraceResponse {
            task: task_val,
            session_id: Some(sess_id.clone()),
            total_events: 0,
            turns_count: 0,
            turns: Vec::new(),
        });
    };

    let reader = BufReader::new(file);
    let mut all_events = Vec::new();
    let mut has_matching_session_start = false;

    for line_res in reader.lines() {
        let Ok(line) = line_res else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<Value>(trimmed) {
            if ev.get("type").and_then(|t| t.as_str()) == Some("session_start") {
                let tid = ev
                    .get("taskId")
                    .or_else(|| ev.get("task_id"))
                    .and_then(|t| t.as_str());
                if tid == Some(task_id) {
                    has_matching_session_start = true;
                }
            }
            all_events.push(ev);
        }
    }

    let target_events: Vec<Value> = if has_matching_session_start {
        let mut filtered = Vec::new();
        let mut inside = false;
        for ev in all_events {
            if ev.get("type").and_then(|t| t.as_str()) == Some("session_start") {
                let tid = ev
                    .get("taskId")
                    .or_else(|| ev.get("task_id"))
                    .and_then(|t| t.as_str());
                inside = tid == Some(task_id);
                if inside {
                    filtered.push(ev);
                }
                continue;
            }
            if inside {
                filtered.push(ev);
            }
        }
        filtered
    } else {
        // Fallback: check if events have taskId directly matching task_id
        let direct_matches: Vec<Value> = all_events
            .iter()
            .filter(|ev| {
                ev.get("taskId")
                    .or_else(|| ev.get("task_id"))
                    .and_then(|t| t.as_str())
                    == Some(task_id)
            })
            .cloned()
            .collect();
        if !direct_matches.is_empty() {
            direct_matches
        } else {
            // Single task session or unpartitioned session: use all events
            all_events
        }
    };

    let mut turns_map: BTreeMap<u32, TurnGroup> = BTreeMap::new();
    let mut total_events = 0;

    for ev in target_events {
        total_events += 1;
        let Some(turn_num) = ev.get("turn").and_then(|t| t.as_u64()) else {
            continue;
        };
        let turn = turn_num as u32;

        let entry = turns_map.entry(turn).or_insert_with(|| TurnGroup {
            turn,
            dispatch: None,
            results: Vec::new(),
            timestamp: None,
        });

        let ev_type = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ev_type == "dispatch" {
            if let Some(ts) = ev.get("timestamp").and_then(|t| t.as_str()) {
                entry.timestamp = Some(ts.to_string());
            }
            entry.dispatch = Some(ev);
        } else if ev_type == "tool_result" {
            entry.results.push(ev);
        }
    }

    let sorted_turns: Vec<TurnGroup> = turns_map.into_values().collect();

    Some(TaskTraceResponse {
        task: task_val,
        session_id: Some(sess_id.clone()),
        total_events,
        turns_count: sorted_turns.len(),
        turns: sorted_turns,
    })
}

/// Read the engine's max context window from `<state>/config.json` for the
/// Context Window Saturation gauge. Prefers the active profile's
/// `max_context`, falling back to the top-level key. Returns `None` when the
/// config is absent, unparseable, or does not set the key (the UI then
/// disables the gauge rather than showing a fake value).
pub fn read_context_config(state: &StateDir) -> Option<u32> {
    let path = state.root().join("config.json");
    let Ok(raw) = fs::read_to_string(&path) else {
        return None;
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return None;
    };
    let as_u32 = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok());

    let profile_name = val.get("profile").and_then(|p| p.as_str());
    let profiles = val.get("profiles");
    if let (Some(name), Some(map)) = (profile_name, profiles)
        && let Some(entry) = map.get(name)
        && let Some(mc) = as_u32(entry.get("max_context").unwrap_or(&Value::Null))
    {
        return Some(mc);
    }
    as_u32(val.get("max_context").unwrap_or(&Value::Null))
}

async fn context_handler(State(state): State<StateDir>) -> impl IntoResponse {
    let resp = ContextConfigResponse {
        max_context: read_context_config(&state),
    };
    (default_headers(), Json(resp))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index_handler() -> impl IntoResponse {
    (default_headers(), Html(TRACE_HTML))
}

async fn poll_handler(
    State(state): State<StateDir>,
    Query(q): Query<PollQuery>,
) -> impl IntoResponse {
    let (recent, total_tasks) = read_recent_tasks(&state, 15);
    let latest = recent.first().cloned();

    let target_task = if let Some(ref req_id) = q.task_id {
        recent
            .iter()
            .find(|t| &t.id == req_id)
            .cloned()
            .or_else(|| {
                find_task_file(&state, req_id).and_then(|p| {
                    let m = p.metadata().ok()?;
                    let mtime = m
                        .modified()
                        .ok()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_millis() as u64;
                    let raw = fs::read_to_string(&p).ok()?;
                    let val: Value = serde_json::from_str(&raw).ok()?;
                    let pid = val.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32);
                    let raw_status = val
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    Some(TaskSummary {
                        id: val
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(req_id)
                            .to_string(),
                        // Same orphan reconciliation as `read_recent_tasks`: a
                        // dead worker pid must never display as a ghost LIVE.
                        status: if is_orphaned(&raw_status, pid) {
                            "orphaned".to_string()
                        } else {
                            raw_status
                        },
                        session_id: val
                            .get("session_id")
                            .or_else(|| val.get("sessionId"))
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        cwd: val.get("cwd").and_then(|v| v.as_str()).map(String::from),
                        prompt: val
                            .get("prompt")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        created_at: val
                            .get("created_at")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(mtime),
                        started_at: val.get("started_at").and_then(|v| v.as_u64()),
                        ended_at: val.get("ended_at").and_then(|v| v.as_u64()),
                        mtime,
                        turns_budget: 0,
                        pid,
                    })
                })
            })
    } else {
        latest.clone()
    };

    let mut mtime = target_task.as_ref().map(|t| t.mtime).unwrap_or(0);
    if let Some(ref task) = target_task
        && let Some(ref sess_id) = task.session_id
    {
        let ev_path = state.sessions().join(sess_id).join("events.jsonl");
        if let Ok(meta) = ev_path.metadata()
            && let Ok(dur) = meta.modified().and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map_err(std::io::Error::other)
            })
        {
            let ev_mtime = dur.as_millis() as u64;
            mtime = mtime.max(ev_mtime);
        }
    }

    let resp = PollResponse {
        active_task_id: target_task.as_ref().map(|t| t.id.clone()),
        status: target_task
            .map(|t| t.status)
            .unwrap_or_else(|| "unknown".to_string()),
        mtime,
        latest_task_id: latest.as_ref().map(|t| t.id.clone()),
        latest_status: latest
            .map(|t| t.status)
            .unwrap_or_else(|| "unknown".to_string()),
        total_tasks,
    };

    (default_headers(), Json(resp))
}

async fn tasks_handler(
    State(state): State<StateDir>,
    Query(q): Query<TasksQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let (tasks, total) = read_recent_tasks(&state, limit);
    (default_headers(), Json(TasksResponse { tasks, total }))
}

async fn task_trace_handler(
    State(state): State<StateDir>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Some(trace) = parse_task_trace(&state, &id) {
        (default_headers(), Json(trace)).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            default_headers(),
            Json(serde_json::json!({ "error": format!("task '{id}' not found") })),
        )
            .into_response()
    }
}

async fn latest_task_handler(State(state): State<StateDir>) -> Response {
    let (recent, _) = read_recent_tasks(&state, 1);
    if let Some(latest) = recent.first()
        && let Some(trace) = parse_task_trace(&state, &latest.id)
    {
        return (default_headers(), Json(trace)).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        default_headers(),
        Json(serde_json::json!({ "error": "no tasks found" })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Router & Server
// ---------------------------------------------------------------------------

/// Build the Axum router for the Castor Trace workbench.
pub fn trace_router(state: StateDir) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/trace", get(index_handler))
        .route("/index.html", get(index_handler))
        .route("/api/poll", get(poll_handler))
        .route("/api/tasks", get(tasks_handler))
        .route("/api/latest", get(latest_task_handler))
        .route("/api/context", get(context_handler))
        .route("/api/task/{id}", get(task_trace_handler))
        .route("/api/task/{id}/turns", get(task_trace_handler))
        .with_state(state)
}

/// Listen and serve the Castor Trace workbench on the given port.
pub async fn serve_trace(state: &StateDir, port: u16) -> std::io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("[castor-trace] visualizer listening on http://127.0.0.1:{port}");
    axum::serve(listener, trace_router(state.clone())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trace_html_is_embedded() {
        assert!(TRACE_HTML.contains("<!DOCTYPE html>"));
        assert!(TRACE_HTML.contains("CASTOR TRACE"));
        let expected_version = format!("v{}", env!("CARGO_PKG_VERSION"));
        assert!(
            TRACE_HTML.contains(&expected_version),
            "trace.html must contain current package version {expected_version}"
        );
    }

    /// Acceptance gate for the frontend upgrades: the embedded workbench must
    /// ship the Follow Live toggle (with its localStorage persistence key) and
    /// the three-tier TPS labeling scheme (aggregate top bar / pure-decode turn
    /// strip / effective raw-log rate).
    #[test]
    fn test_trace_frontend_contract() {
        // Follow Live toggle + persistence key (default on unless 'false').
        assert!(
            TRACE_HTML.contains("btnFollowLive"),
            "header must include the sticky Follow Live toggle"
        );
        assert!(
            TRACE_HTML.contains("castor_trace_follow_live"),
            "follow-live state must persist via localStorage"
        );
        // Dispatch aggregates vs. turn metrics labeling tiers.
        assert!(
            TRACE_HTML.contains("AGG DECODE"),
            "top bar must label the aggregate decode rate"
        );
        assert!(
            TRACE_HTML.contains("PURE DECODE"),
            "turn strip must label the per-turn pure decode rate"
        );
        assert!(
            TRACE_HTML.contains("TURN EFFECTIVE TPS"),
            "raw log / telemetry must label the engine-reported effective rate"
        );
        // High-density visualizations: context gauge, token waterfall, sparklines.
        assert!(TRACE_HTML.contains("ctxGauge"), "context saturation gauge missing");
        assert!(TRACE_HTML.contains("tokenWaterfall"), "token distribution waterfall missing");
        assert!(TRACE_HTML.contains("sparkLatency"), "latency sparkline missing");
        assert!(TRACE_HTML.contains("sparkTps"), "decode throughput sparkline missing");
    }

    /// Write a minimal task record file into a state dir for tests.
    fn write_task(state: &StateDir, id: &str, status: &str, pid: Option<u32>) {
        let val = serde_json::json!({
            "id": id,
            "status": status,
            "prompt": format!("task {id}"),
            "cwd": "/tmp",
            "session_id": format!("sess_{id}"),
            "created_at": 0,
            "turns_budget": 10,
            "pid": pid,
        });
        let path = state.tasks().join(format!("{id}.json"));
        std::fs::create_dir_all(state.tasks()).unwrap();
        std::fs::write(&path, val.to_string()).unwrap();
    }

    fn dead_pid() -> u32 {
        // Spawn and reap an immediately-exiting child, then poll for a pid the
        // kernel is not going to reuse within the test's lifetime.
        let mut child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn `true`");
        child.wait().ok();
        child.id()
    }

    /// An `executing` task whose worker pid is dead must be downgraded to
    /// `orphaned` (never a ghost "LIVE"); terminal tasks and pid-less tasks are
    /// left alone.
    #[test]
    fn test_read_recent_tasks_downgrades_orphaned() {
        let root = std::env::temp_dir().join(format!("castor_trace_orphan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = StateDir::new(&root);

        let pid = dead_pid();
        assert!(!pid_alive(pid), "test precondition: reaped pid must be dead");

        write_task(&state, "orphan", "executing", Some(pid));
        write_task(&state, "done", "completed", None);

        let (tasks, total) = read_recent_tasks(&state, 10);
        assert_eq!(total, 2);

        let orphan = tasks.iter().find(|t| t.id == "orphan").expect("orphan present");
        assert_eq!(
            orphan.status, "orphaned",
            "dead-pid executing task must display as orphaned, not LIVE"
        );

        let done = tasks.iter().find(|t| t.id == "done").expect("done present");
        assert_eq!(done.status, "completed", "terminal tasks must be untouched");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An `executing` task with a *live* pid (this process) must NOT be flagged
    /// as orphaned — the liveness probe is conservative about false positives.
    #[test]
    fn test_is_orphaned_respects_live_pid() {
        let self_pid = std::process::id();
        assert!(pid_alive(self_pid), "our own pid must be alive");
        assert!(!is_orphaned("executing", Some(self_pid)));
        // No pid yet (spawn race): never a false positive.
        assert!(!is_orphaned("executing", None));
        // Terminal statuses are never orphans.
        assert!(!is_orphaned("completed", Some(999_999)));
    }

    /// `/api/context` returns the profile's `max_context` when present and
    /// `None` when the config is absent (gauge disables gracefully).
    #[test]
    fn test_read_context_config() {
        let root = std::env::temp_dir().join(format!("castor_trace_ctx_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = StateDir::new(&root);
        std::fs::create_dir_all(state.root()).unwrap();

        // Absent config → None.
        assert_eq!(read_context_config(&state), None);

        // Top-level max_context.
        std::fs::write(state.root().join("config.json"), r#"{"max_context": 32768}"#).unwrap();
        assert_eq!(read_context_config(&state), Some(32768));

        // Active profile overrides top-level.
        std::fs::write(
            state.root().join("config.json"),
            r#"{"profile":"turbo","max_context":1111,"profiles":{"turbo":{"max_context":245760}}}"#,
        )
        .unwrap();
        assert_eq!(read_context_config(&state), Some(245760));

        let _ = std::fs::remove_dir_all(&root);
    }
}
