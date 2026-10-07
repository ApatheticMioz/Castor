//! State directory retention / pruning.
//!
//! Faithful port of the policy semantics in `mcp-castor/src/state_pruner.js`:
//! - **Sessions**: a session directory is a candidate only when its event
//!   stream carries a terminal event (`session_end` / `session_error`) AND it
//!   is not referenced by an incomplete (not-done) task. Candidates are then
//!   gated by age (default 14 days), count (default 200), and total size
//!   (default 50 MB); the oldest candidates are evicted first.
//! - **Tasks**: a task file is a candidate only when it is terminal (done)
//!   and older than the retention age. Task files referenced by live slot
//!   leases or lockfiles are never candidates.
//! - **Telemetry**: the single `telemetry/events.jsonl` ledger is a candidate
//!   when older than the retention age.
//! - **Evo**: sub-directories under `<state>/evo/` (candidate snapshots) are
//!   candidates when older than the retention age; `lineage.json` itself is
//!   never a candidate.
//!
//! The dry-run gate is explicit: [`plan`] computes what *would* be deleted
//! (a [`PrunePlan`]) and [`apply`] is the only function that deletes. `apply`
//! refuses a plan that was not produced by `plan` (fingerprint mismatch) or
//! that has already been applied (typed error). Nothing is ever deleted
//! implicitly.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Retention policy (defaults ported from `state_pruner.js`).
#[derive(Debug, Clone)]
pub struct PrunePolicy {
    /// Max age in milliseconds (default 14 days).
    pub max_age_ms: u64,
    /// Max number of sessions to keep (default 200).
    pub max_count: usize,
    /// Max total session size in MB (default 50).
    pub max_size_mb: u64,
    /// Override "now" (epoch ms) for deterministic tests.
    pub now: Option<u64>,
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self {
            max_age_ms: 14 * 86_400_000,
            max_count: 200,
            max_size_mb: 50,
            now: None,
        }
    }
}

impl PrunePolicy {
    fn now(&self) -> u64 {
        self.now.unwrap_or_else(now_millis)
    }
}

/// A concrete deletion plan: what *would* be deleted.
///
/// Produced exclusively by [`plan`]; consumed by [`apply`]. The `fingerprint`
/// binds the plan to the exact state it was computed against, and the
/// `applied` flag makes a second `apply` of the same plan a typed error.
#[derive(Debug, Clone)]
pub struct PrunePlan {
    /// The state directory the plan was computed against.
    pub state_dir: PathBuf,
    /// Fingerprint of (state_dir, all paths) — used to detect drift.
    pub fingerprint: u64,
    /// Session directories to remove (`remove_dir_all`).
    pub sessions: Vec<PathBuf>,
    /// Task files to remove (`remove_file`).
    pub tasks: Vec<PathBuf>,
    /// Telemetry files to remove (`remove_file`).
    pub telemetry: Vec<PathBuf>,
    /// Evo sub-directories to remove (`remove_dir_all`).
    pub evo: Vec<PathBuf>,
    /// Set to `true` by [`apply`] once the plan has been executed.
    pub applied: bool,
}

impl PrunePlan {
    /// Total number of paths this plan would delete.
    pub fn len(&self) -> usize {
        self.sessions.len() + self.tasks.len() + self.telemetry.len() + self.evo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Errors from [`apply`].
#[derive(Debug, Error)]
pub enum PruneError {
    /// The plan was not produced by [`plan`] for this state dir (fingerprint
    /// mismatch or state-dir mismatch).
    #[error("plan not produced by plan() for this state dir (fingerprint mismatch)")]
    NotFromPlan,
    /// The plan has already been applied.
    #[error("plan already applied")]
    AlreadyApplied,
    /// An I/O error during deletion (the underlying error is carried
    /// verbatim).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl PartialEq for PruneError {
    fn eq(&self, other: &Self) -> bool {
        use PruneError::*;
        match (self, other) {
            (NotFromPlan, NotFromPlan) => true,
            (AlreadyApplied, AlreadyApplied) => true,
            (Io(a), Io(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

/// Compute a [`PrunePlan`] for `state_dir` under `policy`.
///
/// This is a pure read: nothing is deleted. The returned plan lists exactly
/// the sessions / tasks / telemetry / evo entries that would be deleted.
pub fn plan(state_dir: &Path, policy: &PrunePolicy) -> PrunePlan {
    let now = policy.now();

    let sessions = plan_sessions(state_dir, policy, now);
    let tasks = plan_tasks(state_dir, policy, now);
    let telemetry = plan_telemetry(state_dir, policy, now);
    let evo = plan_evo(state_dir, policy, now);

    let fingerprint = compute_fingerprint(state_dir, &sessions, &tasks, &telemetry, &evo);

    PrunePlan {
        state_dir: state_dir.to_path_buf(),
        fingerprint,
        sessions,
        tasks,
        telemetry,
        evo,
        applied: false,
    }
}

/// Execute a [`PrunePlan`] produced by [`plan`].
///
/// Refuses (with a typed [`PruneError`]) if:
/// - the plan has already been applied, or
/// - the plan's fingerprint does not match the current state (i.e. the plan
///   was not produced by `plan` for this state dir, or the state has drifted).
///
/// Deletion is `remove_dir_all` for sessions/evo and `remove_file` for
/// tasks/telemetry. I/O errors are returned verbatim.
pub fn apply(state_dir: &Path, plan: &mut PrunePlan) -> Result<usize, PruneError> {
    if plan.applied {
        return Err(PruneError::AlreadyApplied);
    }
    if plan.state_dir != state_dir.to_path_buf() {
        return Err(PruneError::NotFromPlan);
    }
    let fp = compute_fingerprint(
        state_dir,
        &plan.sessions,
        &plan.tasks,
        &plan.telemetry,
        &plan.evo,
    );
    if fp != plan.fingerprint {
        return Err(PruneError::NotFromPlan);
    }

    let mut deleted = 0usize;

    for p in &plan.sessions {
        fs::remove_dir_all(p)?;
        deleted += 1;
    }
    for p in &plan.tasks {
        fs::remove_file(p)?;
        deleted += 1;
    }
    for p in &plan.telemetry {
        fs::remove_file(p)?;
        deleted += 1;
    }
    for p in &plan.evo {
        fs::remove_dir_all(p)?;
        deleted += 1;
    }

    plan.applied = true;
    Ok(deleted)
}

// ---------------------------------------------------------------------------
// Session planning (port of `pruneOldSessions`)
// ---------------------------------------------------------------------------

fn plan_sessions(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
    let sessions_dir = state_dir.join("sessions");
    if !sessions_dir.is_dir() {
        return Vec::new();
    }

    let entries = match fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    #[derive(Debug)]
    struct SessionMeta {
        id: String,
        dir: PathBuf,
        mtime: u64,
        size: u64,
        has_terminal: bool,
    }

    let mut sessions: Vec<SessionMeta> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir = entry.path();
        let mtime = session_mtime(&dir);
        let size = dir_size_bytes(&dir);
        let has_terminal = session_has_terminal_event(&dir);
        sessions.push(SessionMeta {
            id: entry.file_name().to_string_lossy().into_owned(),
            dir,
            mtime,
            size,
            has_terminal,
        });
    }

    let active_task_session_ids = active_task_session_ids(state_dir);

    // Classify: protected vs candidates.
    let mut candidates: Vec<&SessionMeta> = Vec::new();
    for s in &sessions {
        // Invariant: never prune active/open sessions.
        if !s.has_terminal {
            continue;
        }
        // Invariant: never prune sessions referenced by incomplete tasks.
        if active_task_session_ids.contains(&s.id) {
            continue;
        }
        candidates.push(s);
    }

    // Sort candidates by mtime ascending (oldest first).
    candidates.sort_by_key(|s| s.mtime);

    let mut to_prune: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // Gate 1: age.
    for s in &candidates {
        if now.saturating_sub(s.mtime) > policy.max_age_ms {
            to_prune.insert(s.id.as_str());
        }
    }

    // Gate 2: count.
    let total_sessions = sessions.len();
    if total_sessions > policy.max_count {
        let mut excess = total_sessions - policy.max_count;
        for s in &candidates {
            if excess == 0 {
                break;
            }
            if !to_prune.contains(s.id.as_str()) {
                to_prune.insert(s.id.as_str());
                excess -= 1;
            }
        }
    }

    // Gate 3: size.
    let total_size: u64 = sessions.iter().map(|s| s.size).sum();
    let max_size_bytes = policy.max_size_mb * 1024 * 1024;
    if total_size > max_size_bytes {
        let mut remaining = total_size;
        for s in &candidates {
            if remaining <= max_size_bytes {
                break;
            }
            if !to_prune.contains(s.id.as_str()) {
                to_prune.insert(s.id.as_str());
                remaining = remaining.saturating_sub(s.size);
            }
        }
    }

    candidates
        .into_iter()
        .filter(|s| to_prune.contains(s.id.as_str()))
        .map(|s| s.dir.clone())
        .collect()
}

/// Returns true when the session's event stream carries a terminal event
/// (`session_end` or `session_error`).
fn session_has_terminal_event(session_dir: &Path) -> bool {
    let log_file = session_dir.join("events.jsonl");
    let content = match fs::read_to_string(&log_file) {
        Ok(c) => c,
        Err(_) => return false,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(t) = ev.get("type").and_then(|x| x.as_str())
            && (t == "session_end" || t == "session_error")
        {
            return true;
        }
    }
    false
}

/// Returns the set of session IDs referenced by incomplete (not-done) tasks
/// on disk.
fn active_task_session_ids(state_dir: &Path) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    let tasks_dir = state_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return ids;
    }
    let entries = match fs::read_dir(&tasks_dir) {
        Ok(e) => e,
        Err(_) => return ids,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        let raw = match fs::read_to_string(entry.path()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let done = v
            .get("status")
            .and_then(|s| s.as_str())
            .map(|s| matches!(s, "completed" | "failed" | "cancelled"))
            .unwrap_or(false);
        if !done && let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
            ids.insert(sid.to_string());
        }
    }
    ids
}

/// mtime of a session directory (uses `events.jsonl` mtime if available,
/// else the directory mtime).
fn session_mtime(session_dir: &Path) -> u64 {
    let log_file = session_dir.join("events.jsonl");
    if let Ok(m) = fs::metadata(&log_file)
        && let Ok(t) = m.modified()
        && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
    {
        return d.as_millis() as u64;
    }
    if let Ok(m) = fs::metadata(session_dir)
        && let Ok(t) = m.modified()
        && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
    {
        return d.as_millis() as u64;
    }
    0
}

/// Total size in bytes of a directory (recursive).
fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let full = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => total += dir_size_bytes(&full),
            Ok(ft) if ft.is_file() => {
                if let Ok(m) = fs::metadata(&full) {
                    total += m.len();
                }
            }
            _ => {}
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Task planning
// ---------------------------------------------------------------------------

fn plan_tasks(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
    let tasks_dir = state_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Vec::new();
    }
    let protected = protected_task_ids(state_dir);

    let entries = match fs::read_dir(&tasks_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json") || name.contains(".tmp_") {
            continue;
        }
        let path = entry.path();
        let raw = match fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Only terminal (done) tasks are candidates.
        let done = v
            .get("status")
            .and_then(|s| s.as_str())
            .map(|s| matches!(s, "completed" | "failed" | "cancelled"))
            .unwrap_or(false);
        if !done {
            continue;
        }
        // Active protection: task files referenced by live slots/locks are
        // never candidates.
        if let Some(id) = v.get("id").and_then(|s| s.as_str())
            && protected.contains(id)
        {
            continue;
        }
        // Age gate.
        let mtime = mtime_of(&path);
        if now.saturating_sub(mtime) > policy.max_age_ms {
            out.push(path);
        }
    }
    out
}

/// Task IDs referenced by live slot leases or lockfiles (never pruned).
fn protected_task_ids(state_dir: &Path) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();

    // Live slot leases: `<state>/tasks/slots/slot_N.json`.
    let slots_dir = state_dir.join("tasks").join("slots");
    if slots_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&slots_dir)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("slot_") || !name.ends_with(".json") {
                continue;
            }
            let raw = match fs::read_to_string(entry.path()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(tid) = v.get("task_id").and_then(|s| s.as_str()) {
                ids.insert(tid.to_string());
            }
        }
    }

    // Lockfiles: `<state>/locks/*.lock`.
    let locks_dir = state_dir.join("locks");
    if locks_dir.is_dir()
        && let Ok(entries) = fs::read_dir(&locks_dir)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".lock") {
                continue;
            }
            let raw = match fs::read_to_string(entry.path()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let v: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(tid) = v.get("task_id").and_then(|s| s.as_str()) {
                ids.insert(tid.to_string());
            }
        }
    }

    ids
}

// ---------------------------------------------------------------------------
// Telemetry planning
// ---------------------------------------------------------------------------

fn plan_telemetry(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
    let events = state_dir.join("telemetry").join("events.jsonl");
    if !events.is_file() {
        return Vec::new();
    }
    let mtime = mtime_of(&events);
    if now.saturating_sub(mtime) > policy.max_age_ms {
        vec![events]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Evo planning
// ---------------------------------------------------------------------------

fn plan_evo(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
    let evo_dir = state_dir.join("evo");
    if !evo_dir.is_dir() {
        return Vec::new();
    }
    let entries = match fs::read_dir(&evo_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir = entry.path();
        let mtime = mtime_of(&dir);
        if now.saturating_sub(mtime) > policy.max_age_ms {
            out.push(dir);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fingerprint
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit over the state dir, a snapshot of the state layout
/// (names + mtimes of entries in each managed sub-directory), and all
/// planned paths.
///
/// Because the layout snapshot is part of the fingerprint, any change to
/// the state dir between `plan` and `apply` (a new session, a new task, a
/// changed mtime) invalidates the plan → [`PruneError::NotFromPlan`].
fn compute_fingerprint(
    state_dir: &Path,
    sessions: &[PathBuf],
    tasks: &[PathBuf],
    telemetry: &[PathBuf],
    evo: &[PathBuf],
) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let prime: u64 = 0x00000100000001b3;

    let mut hash_bytes = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(prime);
        }
    };

    hash_bytes(state_dir.to_string_lossy().as_bytes());

    // Layout snapshot: for each managed sub-directory, hash the sorted
    // (name, mtime) pairs of its entries.
    for sub in ["sessions", "tasks", "telemetry", "evo"] {
        let dir = state_dir.join(sub);
        let mut entries: Vec<(String, u64)> = match fs::read_dir(&dir) {
            Ok(rd) => rd
                .flatten()
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        mtime_of(&e.path()),
                    )
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        entries.sort();
        for (name, mtime) in entries {
            hash_bytes(name.as_bytes());
            hash_bytes(&mtime.to_le_bytes());
        }
    }

    for p in sessions {
        hash_bytes(p.to_string_lossy().as_bytes());
    }
    for p in tasks {
        hash_bytes(p.to_string_lossy().as_bytes());
    }
    for p in telemetry {
        hash_bytes(p.to_string_lossy().as_bytes());
    }
    for p in evo {
        hash_bytes(p.to_string_lossy().as_bytes());
    }
    h
}

/// mtime of a path in epoch ms (0 if unavailable).
fn mtime_of(path: &Path) -> u64 {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let t = match meta.modified() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp_dir() -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-pruner-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_state(root: &Path) {
        fs::create_dir_all(root.join("tasks").join("slots")).unwrap();
        fs::create_dir_all(root.join("sessions")).unwrap();
        fs::create_dir_all(root.join("telemetry")).unwrap();
        fs::create_dir_all(root.join("locks")).unwrap();
        fs::create_dir_all(root.join("evo")).unwrap();
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        let times = std::fs::FileTimes::new().set_modified(t);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
            if let Ok(f) = fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
            {
                let _ = f.set_times(times);
            }
        }
        #[cfg(not(windows))]
        {
            if let Ok(f) = fs::File::open(path) {
                let _ = f.set_times(times);
            }
        }
    }

    /// Create a session dir with an `events.jsonl` carrying a terminal event.
    fn make_session(root: &Path, id: &str, mtime: SystemTime) -> PathBuf {
        let dir = root.join("sessions").join(id);
        fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        let mut f = fs::File::create(&events).unwrap();
        writeln!(f, r#"{{"type":"session_start"}}"#).unwrap();
        writeln!(f, r#"{{"type":"session_end"}}"#).unwrap();
        f.flush().unwrap();
        drop(f);
        set_mtime(&events, mtime);
        set_mtime(&dir, mtime);
        dir
    }

    /// Create a session dir with NO terminal event (active/open).
    fn make_active_session(root: &Path, id: &str, mtime: SystemTime) -> PathBuf {
        let dir = root.join("sessions").join(id);
        fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        let mut f = fs::File::create(&events).unwrap();
        writeln!(f, r#"{{"type":"session_start"}}"#).unwrap();
        f.flush().unwrap();
        drop(f);
        set_mtime(&events, mtime);
        set_mtime(&dir, mtime);
        dir
    }

    /// Create a task file.
    fn make_task(
        root: &Path,
        id: &str,
        status: &str,
        session_id: &str,
        mtime: SystemTime,
    ) -> PathBuf {
        let path = root.join("tasks").join(format!("task_{id}.json"));
        let json = serde_json::json!({
            "id": id,
            "status": status,
            "session_id": session_id,
            "prompt": "test",
            "cwd": "/",
            "heartbeat": 0,
            "turns_budget": 10,
            "created_at": 0,
        });
        fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
        set_mtime(&path, mtime);
        path
    }

    /// Create a slot lease referencing a task.
    fn make_slot(root: &Path, idx: usize, task_id: &str) {
        let path = root
            .join("tasks")
            .join("slots")
            .join(format!("slot_{idx}.json"));
        let json = serde_json::json!({
            "tenant": 999999,
            "task_id": task_id,
            "at": 0,
            "hb": 0,
        });
        fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
    }

    /// A time 30 days before `now` (well past the 14-day retention).
    fn old_time(now: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(now - 30 * 86_400_000)
    }

    /// A time 1 day before `now` (well within the 14-day retention).
    fn new_time(now: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(now - 86_400_000)
    }

    fn policy_now(now: u64) -> PrunePolicy {
        PrunePolicy {
            now: Some(now),
            ..Default::default()
        }
    }

    #[test]
    fn plan_lists_exactly_old_sessions() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000; // 100 days in epoch ms
        let old = old_time(now);
        let new = new_time(now);

        let old1 = make_session(&root, "s_old1", old);
        let old2 = make_session(&root, "s_old2", old);
        let new1 = make_session(&root, "s_new1", new);
        let active = make_active_session(&root, "s_active", old);

        let mut p = plan(&root, &policy_now(now));

        // Exactly the two old sessions.
        assert_eq!(
            p.sessions.len(),
            2,
            "expected 2 old sessions, got {:?}",
            p.sessions
        );
        assert!(p.sessions.contains(&old1));
        assert!(p.sessions.contains(&old2));
        assert!(!p.sessions.contains(&new1));
        assert!(!p.sessions.contains(&active));

        // Apply: deletes old, preserves new + active.
        let n = apply(&root, &mut p).unwrap();
        assert_eq!(n, 2);
        assert!(!old1.exists());
        assert!(!old2.exists());
        assert!(new1.exists());
        assert!(active.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn second_apply_is_typed_refusal() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        let old = old_time(now);
        let s = make_session(&root, "s1", old);

        let mut p = plan(&root, &policy_now(now));
        assert_eq!(p.sessions.len(), 1);
        apply(&root, &mut p).unwrap();
        assert!(!s.exists());

        // Second apply of the same plan → typed refusal.
        let err = apply(&root, &mut p).unwrap_err();
        assert_eq!(err, PruneError::AlreadyApplied);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_state_gives_empty_plan() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        let mut p = plan(&root, &policy_now(now));
        assert!(p.is_empty());
        assert_eq!(p.sessions.len(), 0);
        assert_eq!(p.tasks.len(), 0);
        assert_eq!(p.telemetry.len(), 0);
        assert_eq!(p.evo.len(), 0);

        let n = apply(&root, &mut p).unwrap();
        assert_eq!(n, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn live_slot_task_preserved() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        let old = old_time(now);
        let new = new_time(now);

        // Task referenced by a live slot → protected.
        let live_task = make_task(&root, "t_live", "executing", "sess_live", old);
        make_slot(&root, 0, "t_live");

        // Old done task, not referenced → candidate.
        let old_task = make_task(&root, "t_old", "completed", "sess_old", old);

        // New done task → not old enough.
        let new_task = make_task(&root, "t_new", "completed", "sess_new", new);

        let mut p = plan(&root, &policy_now(now));

        assert!(
            !p.tasks.contains(&live_task),
            "live-slot task must not be a candidate"
        );
        assert!(
            p.tasks.contains(&old_task),
            "old done task should be a candidate"
        );
        assert!(
            !p.tasks.contains(&new_task),
            "new task should not be a candidate"
        );

        let n = apply(&root, &mut p).unwrap();
        assert_eq!(n, 1);
        assert!(live_task.exists(), "live-slot task must survive");
        assert!(!old_task.exists());
        assert!(new_task.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lockfile_task_protected() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        let old = old_time(now);

        // Lockfile referencing a task → protected.
        let lock_path = root.join("locks").join("worker.lock");
        let lock_json = serde_json::json!({
            "pid": 424242,
            "held_at": 0,
            "ttl": 60000,
            "task_id": "t_locked",
        });
        fs::write(&lock_path, serde_json::to_string(&lock_json).unwrap()).unwrap();

        let locked_task = make_task(&root, "t_locked", "completed", "sess_l", old);

        let p = plan(&root, &policy_now(now));
        assert!(
            !p.tasks.contains(&locked_task),
            "lockfile-referenced task must not be a candidate"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_plan_refused() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        let old = old_time(now);
        let s = make_session(&root, "s1", old);

        let mut p = plan(&root, &policy_now(now));
        assert_eq!(p.sessions.len(), 1);

        // Drift: add a new session after the plan was computed.
        let new = new_time(now);
        make_session(&root, "s2", new);

        // Fingerprint no longer matches → NotFromPlan.
        let err = apply(&root, &mut p).unwrap_err();
        assert_eq!(err, PruneError::NotFromPlan);
        assert!(s.exists(), "nothing should have been deleted");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn count_gate_evicts_oldest() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        // 5 sessions, all within age, but max_count = 3 → evict 2 oldest.
        // s0 is the oldest (now-5d), s4 the newest (now-1d).
        let mut paths = Vec::new();
        for i in 0..5 {
            let t = UNIX_EPOCH + Duration::from_millis(now - (5 - i) * 86_400_000);
            paths.push(make_session(&root, &format!("s{i}"), t));
        }

        let policy = PrunePolicy {
            max_count: 3,
            now: Some(now),
            ..Default::default()
        };
        let p = plan(&root, &policy);

        // 5 total, max 3 → evict 2 oldest (s0, s1).
        assert_eq!(
            p.sessions.len(),
            2,
            "expected 2 evicted, got {:?}",
            p.sessions
        );
        assert!(p.sessions.contains(&paths[0]));
        assert!(p.sessions.contains(&paths[1]));
        assert!(!p.sessions.contains(&paths[2]));
        assert!(!p.sessions.contains(&paths[3]));
        assert!(!p.sessions.contains(&paths[4]));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn size_gate_evicts_oldest() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        // 3 sessions, each exactly 1 MB, max_size_mb = 2 → evict oldest until ≤ 2 MB.
        // s0 is the oldest (now-3d), s2 the newest (now-1d).
        let mut paths = Vec::new();
        for i in 0..3 {
            let t = UNIX_EPOCH + Duration::from_millis(now - (3 - i) * 86_400_000);
            let dir = root.join("sessions").join(format!("s{i}"));
            fs::create_dir_all(&dir).unwrap();
            let events = dir.join("events.jsonl");
            let mut f = fs::File::create(&events).unwrap();
            // Write a newline-terminated JSON line (so it parses as a
            // terminal event), then pad so the file is EXACTLY 1 MB.
            let line = format!("{}\n", r#"{"type":"session_end"}"#);
            f.write_all(line.as_bytes()).unwrap();
            let pad = 1024 * 1024 - line.len();
            f.write_all(&vec![b'x'; pad]).unwrap();
            f.flush().unwrap();
            drop(f);
            set_mtime(&events, t);
            set_mtime(&dir, t);
            paths.push(dir);
        }

        let policy = PrunePolicy {
            max_size_mb: 2,
            now: Some(now),
            ..Default::default()
        };
        let p = plan(&root, &policy);

        // 3 × 1 MB = 3 MB > 2 MB → evict oldest (s0) → 2 MB ≤ 2 MB.
        assert_eq!(
            p.sessions.len(),
            1,
            "expected 1 evicted, got {:?}",
            p.sessions
        );
        assert!(p.sessions.contains(&paths[0]));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn evo_old_dir_pruned() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        let old = old_time(now);
        let new = new_time(now);

        let old_evo = root.join("evo").join("cand_old");
        fs::create_dir_all(&old_evo).unwrap();
        set_mtime(&old_evo, old);

        let new_evo = root.join("evo").join("cand_new");
        fs::create_dir_all(&new_evo).unwrap();
        set_mtime(&new_evo, new);

        let mut p = plan(&root, &policy_now(now));
        assert_eq!(p.evo.len(), 1);
        assert!(p.evo.contains(&old_evo));
        assert!(!p.evo.contains(&new_evo));

        let n = apply(&root, &mut p).unwrap();
        assert_eq!(n, 1);
        assert!(!old_evo.exists());
        assert!(new_evo.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn telemetry_old_pruned() {
        let root = tmp_dir();
        make_state(&root);

        let now = 100 * 86_400_000;
        let old = old_time(now);

        let events = root.join("telemetry").join("events.jsonl");
        fs::write(&events, r#"{"level":"INFO"}"#).unwrap();
        set_mtime(&events, old);

        let mut p = plan(&root, &policy_now(now));
        assert_eq!(p.telemetry.len(), 1);
        assert!(p.telemetry.contains(&events));

        let n = apply(&root, &mut p).unwrap();
        assert_eq!(n, 1);
        assert!(!events.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
