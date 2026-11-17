//! Slot lease structures, serialization, and disk path handling.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::StateDir;

/// Unix-epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Liveness probe for a pid (signal 0). Conservative: a probe that errors with
/// EPERM means the process exists (owned by another user) and is treated as
/// alive, so a live worker is never falsely declared dead.
///
/// On Linux a successful `kill(pid, 0)` also reports for **zombie**
/// (`<defunct>`) processes. A zombie has already exited — it cannot execute
/// code, heartbeat, or release its slot — so we re-inspect
/// `/proc/{pid}/status` and treat `State: Z` as dead. Otherwise a defunct
/// worker would keep a slot (e.g. `slot_0.json`) locked forever, deadlocking
/// every other tenant.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() as u32 {
        return true;
    }
    let r = unsafe { libc::kill(pid as i32, 0) };
    if r == 0 {
        #[cfg(target_os = "linux")]
        {
            return !is_linux_zombie(pid);
        }
        #[cfg(not(target_os = "linux"))]
        {
            return true;
        }
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether a Linux pid is a zombie (`State: Z` in `/proc/{pid}/status`).
/// A zombie cannot run code or release a semaphore slot, so it is treated as
/// dead. If `/proc/{pid}/status` is unreadable (process raced away) we
/// conservatively report `false` (not a zombie).
#[cfg(target_os = "linux")]
fn is_linux_zombie(pid: u32) -> bool {
    let path = format!("/proc/{pid}/status");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    content
        .lines()
        .find(|l| l.starts_with("State:"))
        .map(|l| l.trim_start_matches("State:").trim_start().starts_with('Z'))
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !handle.is_null() {
            let mut exit_code: u32 = 0;
            let success = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);
            success != 0 && exit_code == STILL_ACTIVE
        } else {
            false
        }
    }
}

#[cfg(all(not(unix), not(windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

/// A slot lease as persisted on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Owning tenant (process id).
    pub tenant: u32,
    /// Task id bound to this lease (may be empty).
    pub task_id: String,
    /// Epoch ms at which the lease was (re)acquired.
    pub at: u64,
    /// Epoch ms of the last heartbeat.
    pub hb: u64,
    /// Context tokens consumed by the task bound to this lease.
    /// `#[serde(default)]` keeps backward compatibility with lease files
    /// written before this field existed (they deserialize to 0).
    #[serde(default)]
    pub context_tokens: u32,
}

/// Handle to a held slot lease, returned by [`TaskSemaphore::acquire`].
#[derive(Debug, Clone)]
pub struct SlotLease {
    pub index: usize,
    pub file: PathBuf,
    pub tenant: u32,
    pub task_id: String,
}

/// Outcome of a release attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseResult {
    /// A lease we owned (or a dead/unreadable one) was freed.
    Released,
    /// Another live tenant owns the lease; it was not deleted.
    NotOurs,
}

/// Result of a single slot-claim attempt.
#[derive(Debug)]
pub enum ClaimOutcome {
    Claimed(SlotLease),
    Occupied,
    Reclaimed,
}

/// A single slot entry in a [`SlotStatus`].
#[derive(Debug, Clone, PartialEq)]
pub struct SlotEntry {
    pub slot: usize,
    pub lease: Lease,
}

/// Deterministic slot status.
#[derive(Debug, Clone, Default)]
pub struct SlotStatus {
    pub slots: Vec<SlotEntry>,
    pub total_capacity: usize,
    pub alien_holders: Vec<SlotEntry>,
    pub same_holders: Vec<SlotEntry>,
    pub is_alien_active: bool,
    pub is_same_active: bool,
    pub is_fully_occupied: bool,
}

/// Read a lease file; `None` if absent or unparseable.
pub fn read_lease(path: &Path) -> Option<Lease> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Whether a lease is reclaimable: unreadable (crashed mid-write) or owned
/// by a dead pid. A live tenant's lease is NEVER reclaimable.
pub fn lease_reclaimable(lease: Option<&Lease>) -> bool {
    match lease {
        None => true,
        Some(l) => !pid_alive(l.tenant),
    }
}

/// Read the on-disk status of the task a lease is bound to.
///
/// Returns `true` when the bound task has reached a **terminal** state
/// (`completed` / `failed` / `cancelled`), `false` when it is active
/// (`queued` / `executing`), and `None` when the bound task id is empty or
/// the task record is absent / unparseable.
pub fn bound_task_terminal(state: &StateDir, lease: &Lease) -> Option<bool> {
    let task_id = lease.task_id.trim();
    if task_id.is_empty() {
        return None;
    }
    let path = state.tasks().join(format!("{task_id}.json"));
    let raw = fs::read_to_string(&path).ok()?;
    let rec: crate::task::registry::TaskRecord = serde_json::from_str(&raw).ok()?;
    Some(rec.status.is_terminal())
}

/// Full reclamation decision for a lease, combining owner-pid liveness
/// with the bound task's terminal state.
///
/// A lease is reclaimable when:
/// - it is absent / unparseable (a crashed mid-write), OR
/// - its owner pid is dead (including zombies — see [`pid_alive`]), OR
/// - its bound task has reached a **terminal** state.
pub fn lease_reclaimable_full(state: &StateDir, lease: Option<&Lease>) -> bool {
    match lease {
        None => true,
        Some(l) => !pid_alive(l.tenant) || matches!(bound_task_terminal(state, l), Some(true)),
    }
}

/// Write a fully-serialized [`Lease`] to `slot_{i}.json` using
/// write-to-temp-then-`rename`, so a concurrent reader never observes a
/// partial / torn file (only a complete file or, during a claim, a
/// zero-length lock). The temp path is same-directory so the rename is
/// atomic on the same volume; on a rename collision (e.g. cross-device) it
/// falls back to a direct write and removes the temp.
pub fn atomic_publish_lease(slots_dir: &Path, path: &Path, i: usize, lease: &Lease) -> bool {
    let Ok(json) = serde_json::to_string(lease) else {
        return false;
    };
    let tmp = slots_dir.join(format!("slot_{}.{}_tmp", i, std::process::id()));
    if fs::write(&tmp, &json).is_ok() && fs::rename(&tmp, path).is_err() {
        // Rename collided — fall back to a direct write, then clean up.
        let ok = fs::write(path, &json).is_ok();
        let _ = fs::remove_file(&tmp);
        return ok;
    }
    true
}

/// Try to claim a single slot index with a given initial context-token
/// value baked into the persisted [`Lease`].
pub fn try_claim(
    slots_dir: &Path,
    path: &Path,
    i: usize,
    task_id: &str,
    context_tokens: u32,
) -> ClaimOutcome {
    let tenant = std::process::id() as u32;
    let now = now_ms();
    let lease = Lease {
        tenant,
        task_id: task_id.to_string(),
        at: now,
        hb: now,
        context_tokens,
    };

    // 1. Fresh O_EXCL claim (0-byte lock), then atomic publish.
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(_f) => {
            if atomic_publish_lease(slots_dir, path, i, &lease) {
                return ClaimOutcome::Claimed(SlotLease {
                    index: i,
                    file: path.to_path_buf(),
                    tenant,
                    task_id: task_id.to_string(),
                });
            }
            // Publish failed: roll back our lock so the slot is not left
            // half-claimed, then report occupied (caller will retry).
            let _ = fs::remove_file(path);
            return ClaimOutcome::Occupied;
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return ClaimOutcome::Occupied,
    }

    // 2. Exists: a zero-length file is an in-flight claim by a live writer
    //    (O_EXCL already failed with AlreadyExists, so it is not absent).
    //    It must NOT be reclaimed — doing so would let a faster sibling
    //    rename the in-flight file away and the original claimant would
    //    publish to a dead path, ending up with no slot (the classic
    //    "3 of 4 slots visible" flake).
    let st = fs::metadata(path).ok();
    if st.as_ref().is_none_or(|m| m.len() == 0) {
        // Guard against a *stale* lock: if a claimant died between the
        // O_EXCL and the rename, the 0-byte lock would otherwise wedge the
        // slot forever. Reclaim only if the lock is old (>> any publish).
        if let Some(locked_ms) = st.as_ref().and_then(|m| m.modified().ok()) {
            let age_ms = locked_ms
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let now_ms = now_ms();
            if now_ms.saturating_sub(age_ms) > 10_000 {
                let dead = slots_dir.join(format!("slot_{i}.dead_{}_{}", now, tenant));
                if fs::rename(path, &dead).is_ok() {
                    let _ = fs::remove_file(&dead);
                    return ClaimOutcome::Reclaimed;
                }
            }
        }
        return ClaimOutcome::Occupied;
    }

    // 3. Non-zero: a complete (or corrupt) lease. Reclaim only if the
    //    owner is dead (`None` here means non-zero but unparseable, i.e. a
    //    crashed writer).
    let existing = read_lease(path);
    if !lease_reclaimable(existing.as_ref()) {
        return ClaimOutcome::Occupied;
    }
    // Reclaim: atomic rename to a dead file, then remove.
    let dead = slots_dir.join(format!("slot_{i}.dead_{}_{}", now, tenant));
    let _ = fs::rename(path, &dead);
    let _ = fs::remove_file(&dead);
    // Retry the O_EXCL claim once after reclaim, then atomic-publish.
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(_f) => {
            if atomic_publish_lease(slots_dir, path, i, &lease) {
                ClaimOutcome::Claimed(SlotLease {
                    index: i,
                    file: path.to_path_buf(),
                    tenant,
                    task_id: task_id.to_string(),
                })
            } else {
                let _ = fs::remove_file(path);
                ClaimOutcome::Occupied
            }
        }
        Err(_) => ClaimOutcome::Reclaimed,
    }
}

/// **Compare-and-delete** for one slot file: re-read the file and delete
/// it **only if** its `(tenant, task_id, at)` identity still matches the
/// `expected` lease that was evaluated for reclamation (or the file is
/// gone / unreadable). Returns `true` when a file was actually unlinked.
pub fn reap_if_match(path: &Path, expected: Option<&Lease>) -> bool {
    let re_read = read_lease(path);
    if let (Some(a), Some(b)) = (re_read.as_ref(), expected)
        && (a.tenant != b.tenant || a.task_id != b.task_id || a.at != b.at)
    {
        return false; // slot changed since evaluation — do not delete
    }
    fs::remove_file(path).is_ok()
}

/// Reap one slot file end-to-end: evaluate the lease under the full
/// reclamation rule ([`lease_reclaimable_full`]) and, when
/// reclaimable, delete it via the atomic [`reap_if_match`]
/// compare-and-delete. Returns `true` when a file was unlinked.
pub fn reap_slot_file(state: &StateDir, path: &Path) -> bool {
    let lease = read_lease(path);
    if !lease_reclaimable_full(state, lease.as_ref()) {
        return false;
    }
    reap_if_match(path, lease.as_ref())
}
