//! Task registry: in-memory task records with a disk mirror.
//!
//! Core semantics:
//! - A task record is held in an in-memory map behind a `tokio::Mutex`.
//! - Every state transition is mirrored to `<state>/tasks/task_<id>.json`
//!   via write-temp-then-rename (atomic), so a crash mid-write never leaves a
//!   corrupt file.
//! - **Terminal-event idempotence**: a task may reach a terminal state
//!   (completed / failed / cancelled) exactly once; any further terminal
//!   transition is rejected.
//! - **Orphan reaping**: an `executing` task whose worker pid is dead is
//!   transitioned to `failed` with reason `"orphaned worker died"`.
//! - **Elastic budget extension**: `extend_budget` fails fast on a missing or
//!   terminal task and clamps the resulting budget at `MAX_ELASTIC_TURNS`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

use crate::state::StateDir;
use crate::task::semaphore::{pid_alive, TaskSemaphore};

/// Hard cap on the elastic turn budget.
pub const MAX_ELASTIC_TURNS: u32 = 200;

/// Task lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Queued,
    Executing,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    /// Terminal states: completed / failed / cancelled.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Executing => "executing",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        };
        f.write_str(s)
    }
}

/// A single task record (in-memory and on-disk shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub status: TaskStatus,
    /// Terminal reason (e.g. `"orphaned worker died"`); `None` while active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub prompt: String,
    pub cwd: String,
    pub session_id: String,
    /// Worker pid while executing; `None` until the worker is spawned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Epoch ms of the last heartbeat.
    pub heartbeat: u64,
    pub turns_budget: u32,
    /// Epoch ms.
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
}

/// Errors from registry operations.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("task '{id}' not found")]
    NotFound { id: String },
    #[error(
        "task '{id}' is already terminal ({status}); exactly one terminal transition is allowed"
    )]
    AlreadyTerminal { id: String, status: TaskStatus },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl PartialEq for RegistryError {
    fn eq(&self, other: &Self) -> bool {
        use RegistryError::*;
        match (self, other) {
            (NotFound { id: a }, NotFound { id: b }) => a == b,
            (AlreadyTerminal { id: a, status: sa }, AlreadyTerminal { id: b, status: sb }) => {
                a == b && sa == sb
            }
            (Io(_), Io(_)) => true,
            _ => false,
        }
    }
}

/// Outcome of reading a task's disk mirror.
#[derive(Debug)]
pub enum DiskRead {
    /// The task file does not exist.
    NotFound,
    /// The file exists but is corrupt/unparseable (with the parse error).
    Corrupt(String),
    /// A healthy, parsed record.
    Ok(TaskRecord),
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// In-memory task registry with a per-task disk mirror.
///
/// Cheaply cloneable; all clones share the same map.
#[derive(Clone)]
pub struct TaskRegistry {
    state: StateDir,
    inner: Arc<Inner>,
}

static TASK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Convert unix epoch seconds to UTC (year, month, day, hour, min, sec) using
/// Howard Hinnant's civil day algorithm (0 external dependencies).
pub fn epoch_secs_to_utc(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let sec = (secs % 60) as u32;
    let min = ((secs / 60) % 60) as u32;
    let hour = ((secs / 3600) % 24) as u32;
    let mut days = (secs / 86400) as i64;

    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1029 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as u32, m, d, hour, min, sec)
}

/// Sanitize session_id or prompt into a clean, concise slug.
fn sanitize_slug(session_id: &str, prompt: &str) -> String {
    let trimmed = session_id.trim();
    let source =
        if !trimmed.is_empty() && !trimmed.starts_with("castor_session_") && trimmed != "default" {
            trimmed.to_string()
        } else {
            let words: Vec<&str> = prompt.split_whitespace().take(3).collect();
            if words.is_empty() {
                "task".to_string()
            } else {
                words.join("_")
            }
        };

    let mut slug = String::new();
    let mut last_was_underscore = true;
    for c in source.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            slug.push('_');
            last_was_underscore = true;
        }
        if slug.len() >= 16 {
            break;
        }
    }
    let res = slug.trim_end_matches('_');
    if res.is_empty() {
        "task".to_string()
    } else {
        res.to_string()
    }
}

/// Generate a collision-proof task ID:
/// `task_{slug}_{YYYYMMDD_HHMMSS}_{millis:03}_{pid:04x}{seq:04x}`
pub fn generate_task_id(session_id: &str, prompt: &str) -> String {
    let slug = sanitize_slug(session_id, prompt);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let (y, m, d, hh, mm, ss) = epoch_secs_to_utc(now_ms / 1000);
    let millis = (now_ms % 1000) as u32;
    let pid = (std::process::id() & 0xffff) as u16;
    let seq = (TASK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xffff) as u16;
    format!("task_{slug}_{y:04}{m:02}{d:02}_{hh:02}{mm:02}{ss:02}_{millis:03}_{pid:04x}{seq:04x}")
}

struct Inner {
    tasks: Mutex<HashMap<String, TaskRecord>>,
    /// Per-task terminal-transition broadcast channels. A wait handler
    /// subscribes to its task's channel to be woken on a terminal transition.
    terminal_txs: Mutex<HashMap<String, broadcast::Sender<u64>>>,
}

impl TaskRegistry {
    pub fn new(state: &StateDir) -> Self {
        Self {
            state: state.clone(),
            inner: Arc::new(Inner {
                tasks: Mutex::new(HashMap::new()),
                terminal_txs: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn state_dir(&self) -> &StateDir {
        &self.state
    }

    /// Subscribe to a specific task's terminal-transition channel.
    ///
    /// The returned receiver is woken (a value is received) whenever that
    /// task reaches a terminal state. If the task has no channel yet, one is
    /// created on demand.
    pub async fn subscribe_terminal(&self, id: &str) -> broadcast::Receiver<u64> {
        let mut map = self.inner.terminal_txs.lock().await;
        let tx = map
            .entry(id.to_string())
            .or_insert_with(|| broadcast::channel(8).0);
        tx.subscribe()
    }

    /// Path of the disk mirror for a task id.
    pub fn task_path(&self, id: &str) -> PathBuf {
        self.state.tasks().join(format!("{id}.json"))
    }

    /// Atomically write a record to its disk mirror (write-temp-then-rename).
    fn save_to_disk(&self, rec: &TaskRecord) -> std::io::Result<()> {
        let dir = self.state.tasks();
        fs::create_dir_all(&dir)?;
        let path = self.task_path(&rec.id);
        let tmp = dir.join(format!(
            "{}.tmp_{}_{}",
            rec.id,
            std::process::id(),
            now_ms()
        ));
        let json = serde_json::to_string_pretty(rec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)
    }

    /// Create a new task in the `queued` state and mirror it to disk.
    ///
    /// Returns the unique, collision-proof task id (`task_<slug>_<timestamp>_<rand>`).
    pub async fn create(
        &self,
        prompt: impl Into<String>,
        cwd: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<String, RegistryError> {
        let now = now_ms();
        let prompt_str = prompt.into();
        let cwd_str = cwd.into();
        let session_id_str = session_id.into();

        // Guaranteed collision-proof: generate task ID and ensure disk uniqueness.
        let mut id = generate_task_id(&session_id_str, &prompt_str);
        while self.task_path(&id).exists() {
            id = generate_task_id(&session_id_str, &prompt_str);
        }

        let rec = TaskRecord {
            id: id.clone(),
            status: TaskStatus::Queued,
            reason: None,
            prompt: prompt_str,
            cwd: cwd_str,
            session_id: session_id_str,
            pid: None,
            heartbeat: now,
            turns_budget: 0,
            created_at: now,
            started_at: None,
            ended_at: None,
        };
        self.save_to_disk(&rec)?;
        self.inner.tasks.lock().await.insert(id.clone(), rec);
        Ok(id)
    }

    /// Fetch a task by id (disk mirror first to see cross-process updates, then in-memory).
    pub async fn get(&self, id: &str) -> Option<TaskRecord> {
        if let Ok(raw) = fs::read_to_string(self.task_path(id))
            && let Ok(rec) = serde_json::from_str::<TaskRecord>(&raw)
        {
            self.inner
                .tasks
                .lock()
                .await
                .insert(id.to_string(), rec.clone());
            return Some(rec);
        }
        self.inner.tasks.lock().await.get(id).cloned()
    }

    /// Read a task's disk mirror, distinguishing the three execution signals:
    /// the file is absent, the file is corrupt/unparseable, or it is healthy.
    ///
    /// This is the seam the status server uses to return HTTP 400 (never 500)
    /// for a malformed disk file.
    pub fn read_disk(&self, id: &str) -> DiskRead {
        let path = self.task_path(id);
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DiskRead::NotFound,
            Err(e) => return DiskRead::Corrupt(e.to_string()),
        };
        match serde_json::from_str::<TaskRecord>(&raw) {
            Ok(rec) => DiskRead::Ok(rec),
            Err(e) => DiskRead::Corrupt(e.to_string()),
        }
    }

    /// List all known tasks (refreshed from disk mirror).
    pub async fn list(&self) -> Vec<TaskRecord> {
        let mut map = self.inner.tasks.lock().await;
        if let Ok(entries) = fs::read_dir(self.state.tasks()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".json") || !name.starts_with("task_") {
                    continue;
                }
                if let Ok(raw) = fs::read_to_string(entry.path())
                    && let Ok(rec) = serde_json::from_str::<TaskRecord>(&raw)
                {
                    map.insert(rec.id.clone(), rec);
                }
            }
        }
        map.values().cloned().collect()
    }

    /// Apply a mutation to a task and mirror it to disk.
    ///
    /// The closure receives the mutable record; the result is persisted
    /// atomically. Returns the updated record.
    pub async fn update<F>(&self, id: &str, f: F) -> Result<TaskRecord, RegistryError>
    where
        F: FnOnce(&mut TaskRecord),
    {
        let mut map = self.inner.tasks.lock().await;
        if let Ok(raw) = fs::read_to_string(self.task_path(id))
            && let Ok(rec) = serde_json::from_str::<TaskRecord>(&raw)
        {
            map.insert(id.to_string(), rec);
        }
        let rec = map
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound { id: id.to_string() })?;
        f(rec);
        let snapshot = rec.clone();
        drop(map);
        self.save_to_disk(&snapshot)?;
        Ok(snapshot)
    }

    /// Transition a task to a new status.
    ///
    /// - Entering `executing` stamps `started_at` (once).
    /// - Entering a terminal state stamps `ended_at` and stores `reason`.
    /// - **Exactly one terminal transition ever**: if the task is already
    ///   terminal, the transition is rejected with [`RegistryError::AlreadyTerminal`].
    ///
    /// The new state is mirrored to disk.
    pub async fn transition(
        &self,
        id: &str,
        new: TaskStatus,
        reason: Option<String>,
    ) -> Result<TaskRecord, RegistryError> {
        let now = now_ms();
        let mut map = self.inner.tasks.lock().await;
        if let Ok(raw) = fs::read_to_string(self.task_path(id))
            && let Ok(rec) = serde_json::from_str::<TaskRecord>(&raw)
        {
            map.insert(id.to_string(), rec);
        }
        let rec = map
            .get_mut(id)
            .ok_or_else(|| RegistryError::NotFound { id: id.to_string() })?;

        if rec.status.is_terminal() {
            return Err(RegistryError::AlreadyTerminal {
                id: id.to_string(),
                status: rec.status,
            });
        }

        rec.status = new;
        rec.reason = reason;
        rec.heartbeat = now;
        if new == TaskStatus::Executing && rec.started_at.is_none() {
            rec.started_at = Some(now);
        }
        if new.is_terminal() {
            rec.ended_at = Some(now);
        }
        let snapshot = rec.clone();
        drop(map);
        self.save_to_disk(&snapshot)?;
        // Wake any waiters on a terminal transition (per-task broadcast).
        if new.is_terminal() {
            let txs = self.inner.terminal_txs.lock().await;
            if let Some(tx) = txs.get(id) {
                let _ = tx.send(now);
            }
        }
        Ok(snapshot)
    }

    /// Reap orphaned tasks: any `executing` task whose worker pid is dead is
    /// transitioned to `failed` with reason `"orphaned worker died"`.
    ///
    /// Returns the number of tasks reaped. Idempotent: a task already
    /// terminal is never touched (terminal-event idempotence).
    pub async fn reap_orphans(&self) -> usize {
        let candidates: Vec<String> = {
            let map = self.inner.tasks.lock().await;
            map.values()
                .filter(|r| r.status == TaskStatus::Executing)
                .map(|r| r.id.clone())
                .collect()
        };
        let mut reaped = 0;
        for id in candidates {
            let Some(rec) = self.get(&id).await else {
                continue;
            };
            let Some(pid) = rec.pid else {
                continue;
            };
            if pid_alive(pid) {
                continue;
            }
            match self
                .transition(&id, TaskStatus::Failed, Some("orphaned worker died".into()))
                .await
            {
                Ok(_) => reaped += 1,
                Err(RegistryError::AlreadyTerminal { .. }) => {}
                Err(e) => {
                    eprintln!("castor: orphan reap failed for {id}: {e}");
                }
            }
        }
        reaped
    }

    /// Reap orphaned tasks from the on-disk mirror, for startup / boot recovery.
    ///
    /// Unlike [`Self::reap_orphans`] — which only sees tasks this process holds in
    /// memory — this scans `<state>/tasks/*.json` directly. That is the seam that
    /// turns a reboot into honest terminal states: a task left `executing` (or
    /// `queued`) by a process that died without running its shutdown path is
    /// reconciled the next time an MCP instance boots and runs this reaper.
    ///
    /// Reap rules (idempotent: an already-terminal record is never touched):
    /// - `executing` with a worker pid that is dead → `failed` /
    ///   `"process terminated / system reboot"`. A *live* pid is always
    ///   respected (never reaped), so a healthy long-running worker is never
    ///   false-positived.
    /// - `executing` with *no* pid but a heartbeat older than 60 s → `failed` /
    ///   `"process terminated / system reboot"` (the worker died before it ever
    ///   stamped its identity; a 60 s grace covers a fresh transition).
    /// - `queued` with no pid whose `created_at` is older than 5 minutes →
    ///   `cancelled` / `"worker never started / process terminated"`.
    ///
    /// Returns the number of tasks reaped.
    pub async fn reap_orphans_disk(&self) -> usize {
        const STALE_HEARTBEAT_MS: u64 = 60_000; // 60 s
        const STALE_QUEUE_MS: u64 = 5 * 60 * 1000; // 5 min
        let now = now_ms();
        let dir = self.state.tasks();
        let Ok(entries) = fs::read_dir(&dir) else {
            return 0;
        };
        let mut reaped = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("task_") || !name.ends_with(".json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut rec) = serde_json::from_str::<TaskRecord>(&raw) else {
                continue;
            };
            if rec.status.is_terminal() {
                continue;
            }
            match rec.status {
                TaskStatus::Executing => {
                    let pid = rec.pid;
                    match pid {
                        // No worker identity: only reap once the heartbeat has
                        // gone stale (a freshly transitioned task gets 60 s).
                        None if now.saturating_sub(rec.heartbeat) < STALE_HEARTBEAT_MS => continue,
                        None => {
                            rec.status = TaskStatus::Failed;
                            rec.reason = Some("process terminated / system reboot".into());
                            rec.ended_at = Some(now);
                        }
                        // Live worker: never reap (avoids false-positiving a
                        // healthy long-running task).
                        Some(p) if pid_alive(p) => continue,
                        Some(_) => {
                            rec.status = TaskStatus::Failed;
                            rec.reason = Some("process terminated / system reboot".into());
                            rec.ended_at = Some(now);
                        }
                    }
                }
                TaskStatus::Queued if rec.pid.is_none() => {
                    if now.saturating_sub(rec.created_at) < STALE_QUEUE_MS {
                        continue;
                    }
                    rec.status = TaskStatus::Cancelled;
                    rec.reason = Some("worker never started / process terminated".into());
                    rec.ended_at = Some(now);
                }
                _ => continue,
            }
            if self.save_to_disk(&rec).is_err() {
                eprintln!("castor: failed to persist reaped task {}", rec.id);
            }
            reaped += 1;
        }
        reaped
    }

    /// Reap orphaned **slot** leases: delete any `<state>/tasks/slots/slot_N.json`
    /// whose owner pid is dead (including zombies) or whose bound task has
    /// reached a terminal state (completed / failed / cancelled).
    ///
    /// This is the zombie-slot reaper. A worker that dies (panic, kill, crash)
    /// after marking its task terminal but before releasing its slot leaves the
    /// lease file on disk; while that worker's pid still exists the plain
    /// dead-owner check would not reclaim the slot, deadlocking every other
    /// tenant. Checking the bound task's terminal state reclaims such slots
    /// promptly. A live worker whose task is still active is never reaped, so a
    /// healthy long-running task is never false-positived.
    ///
    /// Returns the number of slot leases removed. Idempotent.
    pub async fn reap_slots(&self) -> usize {
        TaskSemaphore::new(&self.state, 1).reap_slots()
    }

    /// Extend the turn budget of an active task by `n` turns.
    ///
    /// Fails fast with [`RegistryError::NotFound`] if the task does not exist
    /// and with [`RegistryError::AlreadyTerminal`] if it is terminal. The
    /// resulting budget is clamped at [`MAX_ELASTIC_TURNS`].
    ///
    /// Returns the new budget.
    pub async fn extend_budget(&self, id: &str, n: u32) -> Result<u32, RegistryError> {
        let new_budget = {
            let mut map = self.inner.tasks.lock().await;
            let rec = map
                .get_mut(id)
                .ok_or_else(|| RegistryError::NotFound { id: id.to_string() })?;
            if rec.status.is_terminal() {
                return Err(RegistryError::AlreadyTerminal {
                    id: id.to_string(),
                    status: rec.status,
                });
            }
            rec.turns_budget = (rec.turns_budget + n).min(MAX_ELASTIC_TURNS);
            rec.heartbeat = now_ms();
            rec.turns_budget
        };
        let rec = self.get(id).await.expect("just updated");
        self.save_to_disk(&rec)?;
        Ok(new_budget)
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
        let p = std::env::temp_dir().join(format!("castor-reg-{}-{}", std::process::id(), n));
        let s = StateDir::new(p);
        s.ensure().unwrap();
        s
    }

    /// A pid that is (almost certainly) dead: spawn a child and wait for it.
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

    #[tokio::test]
    async fn create_get_list_update() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let id = reg.create("do the thing", "/tmp", "sess-1").await.unwrap();
        assert!(id.starts_with("task_"));

        let rec = reg.get(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Queued);
        assert_eq!(rec.prompt, "do the thing");
        assert_eq!(rec.session_id, "sess-1");

        // Disk mirror exists and round-trips.
        let path = reg.task_path(&id);
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), format!("{id}.json").as_str());
        let raw = fs::read_to_string(&path).unwrap();
        let from_disk: TaskRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(from_disk.id, id);

        // Update.
        let updated = reg
            .update(&id, |r| {
                r.pid = Some(1234);
            })
            .await
            .unwrap();
        assert_eq!(updated.pid, Some(1234));

        // List contains it.
        let all = reg.list().await;
        assert!(all.iter().any(|r| r.id == id));
    }

    #[tokio::test]
    async fn transition_stamps_times_and_mirrors() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let id = reg.create("p", "/tmp", "s").await.unwrap();

        let before = now_ms();
        let rec = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();
        assert_eq!(rec.status, TaskStatus::Executing);
        assert!(rec.started_at.is_some());
        assert!(rec.ended_at.is_none());
        assert!(rec.started_at.unwrap() >= before);

        let rec = reg
            .transition(&id, TaskStatus::Completed, None)
            .await
            .unwrap();
        assert!(rec.ended_at.is_some());

        // Disk mirror reflects the terminal state.
        let raw = fs::read_to_string(reg.task_path(&id)).unwrap();
        let from_disk: TaskRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(from_disk.status, TaskStatus::Completed);
        assert!(from_disk.ended_at.is_some());
    }

    #[tokio::test]
    async fn terminal_exactly_once() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let id = reg.create("p", "/tmp", "s").await.unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Failed, Some("boom".into()))
            .await
            .unwrap();

        // Second terminal transition is rejected.
        let err = reg
            .transition(&id, TaskStatus::Cancelled, Some("late cancel".into()))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            RegistryError::AlreadyTerminal {
                id: id.clone(),
                status: TaskStatus::Failed
            }
        );

        // And the record is unchanged.
        let rec = reg.get(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Failed);
        assert_eq!(rec.reason.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn orphan_reap_on_dead_pid() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let id = reg.create("p", "/tmp", "s").await.unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();
        let dead = dead_pid();
        let _ = reg.update(&id, |r| r.pid = Some(dead)).await.unwrap();

        let reaped = reg.reap_orphans().await;
        assert_eq!(reaped, 1);
        let rec = reg.get(&id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Failed);
        assert_eq!(rec.reason.as_deref(), Some("orphaned worker died"));

        // Idempotent: a second pass reaps nothing.
        assert_eq!(reg.reap_orphans().await, 0);
    }

    #[tokio::test]
    async fn orphan_reap_skips_live_pid() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let id = reg.create("p", "/tmp", "s").await.unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();
        // Our own pid is alive.
        let _ = reg
            .update(&id, |r| r.pid = Some(std::process::id() as u32))
            .await
            .unwrap();
        assert_eq!(reg.reap_orphans().await, 0);
        assert_eq!(reg.get(&id).await.unwrap().status, TaskStatus::Executing);
    }

    /// Write a raw task record straight to the disk mirror (bypassing the
    /// registry) so the boot reaper's input can carry arbitrary timestamps /
    /// pids that the live lifecycle would not permit.
    fn write_raw_task(state: &StateDir, id: &str, rec: &TaskRecord) {
        let dir = state.tasks();
        let _ = fs::create_dir_all(&dir);
        let json = serde_json::to_string_pretty(rec).unwrap();
        fs::write(dir.join(format!("{id}.json")), json).unwrap();
    }

    #[tokio::test]
    async fn reap_orphans_disk_reboots_orphans_to_terminal() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let now = now_ms();
        let old = now - 3_600_000; // 1 h ago (well past both the 5 min and 60 s thresholds)
        let dead = dead_pid();

        let a = generate_task_id("sess_a", "p"); // executing, dead pid → failed
        let b = generate_task_id("sess_b", "p"); // queued, old, no pid → cancelled
        let c = generate_task_id("sess_c", "p"); // queued, fresh, no pid → kept
        let d = generate_task_id("sess_d", "p"); // executing, live pid → kept
        let e = generate_task_id("sess_e", "p"); // completed (terminal) → untouched

        write_raw_task(
            &state,
            &a,
            &TaskRecord {
                id: a.clone(),
                status: TaskStatus::Executing,
                reason: None,
                prompt: "p".into(),
                cwd: "/tmp".into(),
                session_id: "a".into(),
                pid: Some(dead),
                heartbeat: old,
                turns_budget: 0,
                created_at: old,
                started_at: Some(old),
                ended_at: None,
            },
        );
        write_raw_task(
            &state,
            &b,
            &TaskRecord {
                id: b.clone(),
                status: TaskStatus::Queued,
                reason: None,
                prompt: "p".into(),
                cwd: "/tmp".into(),
                session_id: "b".into(),
                pid: None,
                heartbeat: old,
                turns_budget: 0,
                created_at: old,
                started_at: None,
                ended_at: None,
            },
        );
        write_raw_task(
            &state,
            &c,
            &TaskRecord {
                id: c.clone(),
                status: TaskStatus::Queued,
                reason: None,
                prompt: "p".into(),
                cwd: "/tmp".into(),
                session_id: "c".into(),
                pid: None,
                heartbeat: now,
                turns_budget: 0,
                created_at: now,
                started_at: None,
                ended_at: None,
            },
        );
        write_raw_task(
            &state,
            &d,
            &TaskRecord {
                id: d.clone(),
                status: TaskStatus::Executing,
                reason: None,
                prompt: "p".into(),
                cwd: "/tmp".into(),
                session_id: "d".into(),
                pid: Some(std::process::id() as u32),
                heartbeat: now,
                turns_budget: 0,
                created_at: now,
                started_at: Some(now),
                ended_at: None,
            },
        );
        write_raw_task(
            &state,
            &e,
            &TaskRecord {
                id: e.clone(),
                status: TaskStatus::Completed,
                reason: None,
                prompt: "p".into(),
                cwd: "/tmp".into(),
                session_id: "e".into(),
                pid: Some(dead),
                heartbeat: old,
                turns_budget: 0,
                created_at: old,
                started_at: Some(old),
                ended_at: Some(old),
            },
        );

        // Boot reaper: reaps exactly the two genuine orphans (a, b); leaves c,
        // d, and the terminal e alone.
        assert_eq!(reg.reap_orphans_disk().await, 2, "only the two orphans should be reaped");

        let ra = reg.get(&a).await.unwrap();
        assert_eq!(ra.status, TaskStatus::Failed);
        assert_eq!(ra.reason.as_deref(), Some("process terminated / system reboot"));
        assert!(ra.ended_at.is_some());

        let rb = reg.get(&b).await.unwrap();
        assert_eq!(rb.status, TaskStatus::Cancelled);
        assert_eq!(rb.reason.as_deref(), Some("worker never started / process terminated"));
        assert!(rb.ended_at.is_some());

        let rc = reg.get(&c).await.unwrap();
        assert_eq!(rc.status, TaskStatus::Queued);
        assert!(rc.reason.is_none());

        let rd = reg.get(&d).await.unwrap();
        assert_eq!(rd.status, TaskStatus::Executing);
        assert!(rd.reason.is_none());

        let re = reg.get(&e).await.unwrap();
        assert_eq!(re.status, TaskStatus::Completed);

        // Idempotent: a second pass reaps nothing new.
        assert_eq!(reg.reap_orphans_disk().await, 0, "a second boot pass must be a no-op");
    }

    /// The slot reaper must reclaim orphaned slot leases two ways:
    /// (a) the bound task has reached a terminal state (a worker that died
    /// after marking its task terminal but before releasing its slot), and
    /// (b) the owner pid is dead. A live worker whose task is still active must
    /// never be reclaimed.
    #[tokio::test]
    async fn reap_slots_reclaims_terminal_task_and_dead_owner_leases() {
        use crate::task::semaphore::{Lease, TaskSemaphore};

        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let sem = TaskSemaphore::new(&state, 4);
        let now = now_ms();
        let me = std::process::id() as u32;
        let dead = dead_pid();

        // Create a real completed task (disk mirror present + terminal).
        let done = reg.create("p", "/tmp", "s").await.unwrap();
        let _ = reg
            .transition(&done, TaskStatus::Executing, None)
            .await
            .unwrap();
        let _ = reg
            .transition(&done, TaskStatus::Completed, None)
            .await
            .unwrap();

        // Slot 0: bound to a *terminal* task, owned by a LIVE pid (ourself).
        // The dead-owner rule alone would NOT reclaim this, but the
        // terminal-task rule must.
        fs::write(
            sem.slot_path(0),
            serde_json::to_string(&Lease {
                tenant: me,
                task_id: done.clone(),
                at: now,
                hb: now,
                context_tokens: 0,
            })
            .unwrap(),
        )
        .unwrap();

        // Slot 1: bound to an *active* (executing) task, owned by a live pid.
        // Must NOT be reclaimed (the live-worker invariant).
        let active = reg.create("p", "/tmp", "s").await.unwrap();
        let _ = reg
            .transition(&active, TaskStatus::Executing, None)
            .await
            .unwrap();
        let _ = reg.update(&active, |r| r.pid = Some(me)).await.unwrap();
        fs::write(
            sem.slot_path(1),
            serde_json::to_string(&Lease {
                tenant: me,
                task_id: active.clone(),
                at: now,
                hb: now,
                context_tokens: 0,
            })
            .unwrap(),
        )
        .unwrap();

        // Slot 2: bound to a *nonexistent* task, owned by a DEAD pid.
        // The dead-owner rule must reclaim it (and the missing task record
        // must not be mistaken for terminal).
        fs::write(
            sem.slot_path(2),
            serde_json::to_string(&Lease {
                tenant: dead,
                task_id: "task_ghost_never_created".into(),
                at: now,
                hb: now,
                context_tokens: 0,
            })
            .unwrap(),
        )
        .unwrap();

        let reaped = reg.reap_slots().await;
        assert_eq!(reaped, 2, "only slot 0 (terminal task) and slot 2 (dead owner) should be reclaimed");
        assert!(
            !sem.slot_path(0).exists(),
            "terminal-task slot must be reclaimed"
        );
        assert!(
            sem.slot_path(1).exists(),
            "live-worker / active-task slot must be kept"
        );
        assert!(
            !sem.slot_path(2).exists(),
            "dead-owner slot must be reclaimed"
        );

        // Idempotent: a second pass reaps nothing.
        assert_eq!(reg.reap_slots().await, 0);
    }

    #[tokio::test]
    async fn extend_budget_clamps_and_fail_fast() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);
        let id = reg.create("p", "/tmp", "s").await.unwrap();
        let _ = reg
            .transition(&id, TaskStatus::Executing, None)
            .await
            .unwrap();

        // Normal extension.
        let b = reg.extend_budget(&id, 30).await.unwrap();
        assert_eq!(b, 30);

        // Clamp at MAX_ELASTIC_TURNS.
        let b = reg.extend_budget(&id, 10_000).await.unwrap();
        assert_eq!(b, MAX_ELASTIC_TURNS);

        // Fail fast: missing task.
        let err = reg.extend_budget("nope", 5).await.unwrap_err();
        assert_eq!(err, RegistryError::NotFound { id: "nope".into() });

        // Fail fast: terminal task.
        let _ = reg
            .transition(&id, TaskStatus::Completed, None)
            .await
            .unwrap();
        let err = reg.extend_budget(&id, 5).await.unwrap_err();
        assert_eq!(
            err,
            RegistryError::AlreadyTerminal {
                id: id.clone(),
                status: TaskStatus::Completed
            }
        );
    }

    #[test]
    fn civil_calendar_date_conversion() {
        // 1970-01-01 00:00:00 UTC
        assert_eq!(epoch_secs_to_utc(0), (1970, 1, 1, 0, 0, 0));

        // Leap year: 1709214645
        assert_eq!(epoch_secs_to_utc(1709214645), (2024, 2, 29, 13, 50, 45));

        // Future epoch seconds: 1791112159
        assert_eq!(epoch_secs_to_utc(1791112159), (2026, 10, 4, 11, 9, 19));
    }

    #[tokio::test]
    async fn task_id_format_and_collision_resistance() {
        let state = tmp_state();
        let reg = TaskRegistry::new(&state);

        let id = reg
            .create("write question 1", "/tmp", "rm_activity_02")
            .await
            .unwrap();
        assert!(id.starts_with("task_rm_activity_02_"));
        // task_{slug}_{YYYYMMDD_HHMMSS}_{millis:03}_{pid:04x}{seq:04x}
        let parts: Vec<&str> = id.split('_').collect();
        assert!(parts.len() >= 6); // task, rm, activity, 02, date, time, millis, pidseq
        let last = parts.last().unwrap();
        assert_eq!(
            last.len(),
            8,
            "last component should be 8 hex chars (4 pid + 4 seq)"
        );

        // Concurrent generation of 1,000 tasks: guaranteed 0 collisions
        let mut set = std::collections::HashSet::new();
        for _ in 0..1000 {
            let tid = generate_task_id("test_sess", "some prompt");
            assert!(set.insert(tid), "collision detected!");
        }
        assert_eq!(set.len(), 1000);
    }
}
