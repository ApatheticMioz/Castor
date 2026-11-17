//! Filesystem traversal, inspection, fingerprinting, and deletion routines for state pruning.

use std::fs;
use std::path::{Path, PathBuf};

use super::{AUTO_PRUNE_INTERVAL_MS, LAST_PRUNE_FILE, PrunePolicy};

/// Epoch milliseconds helper.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// mtime of a path in epoch ms (0 if unavailable).
pub fn mtime_of(path: &Path) -> u64 {
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

// ---------------------------------------------------------------------------
// Session planning
// ---------------------------------------------------------------------------

pub fn plan_sessions(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
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

pub fn plan_tasks(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
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

pub fn plan_telemetry(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
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

pub fn plan_evo(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
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
// Scratch planning & pruning
// ---------------------------------------------------------------------------

/// Compute candidate paths directly under `scratch_dir` older than `ttl_ms`.
pub fn plan_scratch(scratch_dir: &Path, ttl_ms: u64, now: u64) -> Vec<PathBuf> {
    if !scratch_dir.is_dir() {
        return Vec::new();
    }
    let entries = match fs::read_dir(scratch_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let mtime = mtime_of(&path);
        if now.saturating_sub(mtime) > ttl_ms {
            out.push(path);
        }
    }
    out
}

/// Prune all items directly under `scratch_dir` older than `ttl_ms` (defaults to 4 days).
/// Recursively deletes directories and removes files. Returns the count of deleted items.
pub fn prune_scratch_dir(
    scratch_dir: &Path,
    ttl_ms: u64,
    now: Option<u64>,
) -> Result<usize, std::io::Error> {
    if !scratch_dir.is_dir() {
        return Ok(0);
    }
    let now_val = now.unwrap_or_else(now_millis);
    let candidates = plan_scratch(scratch_dir, ttl_ms, now_val);
    let mut count = 0;
    for p in candidates {
        if p.is_dir() {
            fs::remove_dir_all(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
        count += 1;
    }
    Ok(count)
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
pub fn compute_fingerprint(
    state_dir: &Path,
    sessions: &[PathBuf],
    tasks: &[PathBuf],
    telemetry: &[PathBuf],
    evo: &[PathBuf],
    scratch: &[PathBuf],
    tombstones: &[PathBuf],
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
    for sub in ["sessions", "tasks", "telemetry", "evo", ".scratch"] {
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
    for p in scratch {
        hash_bytes(p.to_string_lossy().as_bytes());
    }
    for p in tombstones {
        hash_bytes(p.to_string_lossy().as_bytes());
    }
    h
}

// ---------------------------------------------------------------------------
// Tombstone planning
// ---------------------------------------------------------------------------

pub fn plan_tombstones(state_dir: &Path, policy: &PrunePolicy, now: u64) -> Vec<PathBuf> {
    let locks_dir = state_dir.join("locks");
    if !locks_dir.is_dir() {
        return Vec::new();
    }
    let entries = match fs::read_dir(&locks_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".tombstone") {
            continue;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        // Try reading lock metadata { pid, held_at, ttl_ms }
        let is_expired = if let Ok(raw) = fs::read_to_string(&path)
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(held_at) = v.get("held_at").and_then(|h| h.as_u64())
            && let Some(ttl_ms) = v.get("ttl_ms").and_then(|t| t.as_u64())
        {
            now.saturating_sub(held_at) > ttl_ms
        } else {
            // Unreadable or non-JSON: check mtime against max_age or default 1 hour
            let mtime = mtime_of(&path);
            now.saturating_sub(mtime) > policy.max_age_ms.min(3_600_000)
        };

        if is_expired {
            out.push(path);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Auto-prune lifecycle
// ---------------------------------------------------------------------------

/// Check if `.last_prune` in `state_dir` is older than 24 hours (or missing).
pub fn needs_auto_prune(state_dir: &Path, now_ms: u64) -> bool {
    let last_prune_path = state_dir.join(LAST_PRUNE_FILE);
    if !last_prune_path.exists() {
        return true;
    }
    match fs::read_to_string(&last_prune_path) {
        Ok(s) => match s.trim().parse::<u64>() {
            Ok(last_ms) => now_ms.saturating_sub(last_ms) >= AUTO_PRUNE_INTERVAL_MS,
            Err(_) => {
                let mtime = mtime_of(&last_prune_path);
                now_ms.saturating_sub(mtime) >= AUTO_PRUNE_INTERVAL_MS
            }
        },
        Err(_) => true,
    }
}

/// Record the current timestamp in `.last_prune`.
pub fn record_last_prune(state_dir: &Path, now_ms: u64) -> std::io::Result<()> {
    let last_prune_path = state_dir.join(LAST_PRUNE_FILE);
    fs::write(last_prune_path, now_ms.to_string())
}

/// Spawns an automatic, non-blocking background pruning pass on boot if `.last_prune`
/// is older than 24 hours.
/// Returns a `JoinHandle` if a thread was spawned, or `None` if pruning was not needed.
pub fn maybe_auto_prune(state_dir: &Path) -> Option<std::thread::JoinHandle<()>> {
    let now = now_millis();
    if !needs_auto_prune(state_dir, now) {
        return None;
    }
    let state_dir_buf = state_dir.to_path_buf();
    std::thread::Builder::new()
        .name("castor-auto-prune".into())
        .spawn(move || {
            let locks = crate::state::Locks::new(state_dir_buf.join("locks"));
            let lock_name = "pruner";
            if let Ok(crate::state::AcquireResult::Acquired) = locks.acquire(lock_name, 300_000) {
                let now_inner = now_millis();
                if needs_auto_prune(&state_dir_buf, now_inner) {
                    let policy = super::PrunePolicy::default();
                    let mut p = super::plan(&state_dir_buf, &policy);
                    if !p.is_empty() {
                        let _ = super::apply(&state_dir_buf, &mut p);
                    }
                    let _ = record_last_prune(&state_dir_buf, now_inner);
                }
                let _ = locks.release(lock_name);
            }
        })
        .ok()
}
