//! Cross-process task-slot semaphore.
//!
//! Faithful port of `mcp-castor/src/semaphore.js`. Slots are file leases at
//! `<state>/tasks/slots/slot_N.json` (N in `0..max_slots`). A lease is claimed
//! with an atomic O_EXCL create, heartbeated by rewriting the file, and freed
//! by an ownership-checked unlink.
//!
//! Semantics preserved:
//! - **Single-tenant multi-slot invariant**: a tenant (pid) may hold up to
//!   `max_slots` slots, but if an *alien* live tenant holds ANY slot, this
//!   tenant is blocked until the alien releases all of its slots.
//! - **FIFO when full**: in-process waiters are queued in a FIFO wait-queue;
//!   a release wakes the head waiter first. Cross-process changes are picked
//!   up by a poll timeout.
//! - **Stale-lease reclaim**: a lease whose owner pid is dead is reclaimable
//!   (the live-owner invariant: a live tenant's lease is never reclaimed).
//! - **Zero leftover leases**: release unlinks the lease file; a double
//!   release is an idempotent no-op.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::time::sleep;

use crate::state::StateDir;

/// Default poll cadence (ms) when waiting to acquire a slot.
const DEFAULT_POLL_MS: u64 = 1000;

/// Unix-epoch milliseconds.
fn now_ms() -> u64 {
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
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !handle.is_null() {
            CloseHandle(handle);
            true
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
enum ClaimOutcome {
    Claimed(SlotLease),
    Occupied,
    Reclaimed,
}

/// In-process FIFO wait-queue state (guarded by a single std Mutex so that
/// both the async `acquire` and the sync `release` can manipulate it without
/// holding a lock across an await).
#[derive(Default)]
struct Waiters {
    queue: VecDeque<u64>,
    channels: HashMap<u64, oneshot::Sender<()>>,
}

/// Cross-process task-slot semaphore.
///
/// Cheaply cloneable (all shared state lives behind an `Arc`); clones share the
/// same slots dir, capacity, and in-process wait-queue.
#[derive(Clone)]
pub struct TaskSemaphore {
    inner: Arc<Inner>,
}

struct Inner {
    slots_dir: PathBuf,
    max_slots: usize,
    poll_interval: Duration,
    waiters: Mutex<Waiters>,
    next_id: AtomicU64,
}

impl TaskSemaphore {
    /// Build a semaphore over `state` with `max_slots` slots (clamped to >= 1)
    /// and the default 1s poll interval.
    pub fn new(state: &StateDir, max_slots: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots_dir: state.task_slots(),
                max_slots: max_slots.max(1),
                poll_interval: Duration::from_millis(DEFAULT_POLL_MS),
                waiters: Mutex::new(Waiters::default()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// Build with an explicit poll interval (used by tests to speed up the
    /// cross-process hand-off).
    pub fn with_poll_interval(state: &StateDir, max_slots: usize, poll_ms: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots_dir: state.task_slots(),
                max_slots: max_slots.max(1),
                poll_interval: Duration::from_millis(poll_ms.max(1)),
                waiters: Mutex::new(Waiters::default()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    pub fn max_slots(&self) -> usize {
        self.inner.max_slots
    }

    pub fn slots_dir(&self) -> &PathBuf {
        &self.inner.slots_dir
    }

    fn slot_path(&self, i: usize) -> PathBuf {
        self.inner.slots_dir.join(format!("slot_{i}.json"))
    }

    /// Read a lease file; `None` if absent or unparseable.
    fn read_lease(&self, i: usize) -> Option<Lease> {
        let raw = fs::read_to_string(self.slot_path(i)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Whether a lease is reclaimable: unreadable (crashed mid-write) or owned
    /// by a dead pid. A live tenant's lease is NEVER reclaimable.
    fn lease_reclaimable(&self, lease: Option<&Lease>) -> bool {
        match lease {
            None => true,
            Some(l) => !pid_alive(l.tenant),
        }
    }

    /// Try to claim a single slot index.
    fn try_claim(&self, i: usize, task_id: &str) -> ClaimOutcome {
        let path = self.slot_path(i);
        let tenant = std::process::id() as u32;
        let now = now_ms();

        // 1. Fresh O_EXCL claim.
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut f) => {
                let lease = Lease {
                    tenant,
                    task_id: task_id.to_string(),
                    at: now,
                    hb: now,
                };
                if let Ok(json) = serde_json::to_string(&lease) {
                    let _ = write_all(&mut f, json.as_bytes());
                }
                return ClaimOutcome::Claimed(SlotLease {
                    index: i,
                    file: path,
                    tenant,
                    task_id: task_id.to_string(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return ClaimOutcome::Occupied,
        }

        // 2. Exists: reclaim if the owner is dead, else occupied.
        let lease = self.read_lease(i);
        if !self.lease_reclaimable(lease.as_ref()) {
            return ClaimOutcome::Occupied;
        }
        // Reclaim: atomic rename to a dead file, then remove.
        let dead = self
            .inner
            .slots_dir
            .join(format!("slot_{i}.dead_{}_{}", now, tenant));
        let _ = fs::rename(&path, &dead);
        let _ = fs::remove_file(&dead);
        // Retry the O_EXCL claim once after reclaim.
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut f) => {
                let lease = Lease {
                    tenant,
                    task_id: task_id.to_string(),
                    at: now,
                    hb: now,
                };
                if let Ok(json) = serde_json::to_string(&lease) {
                    let _ = write_all(&mut f, json.as_bytes());
                }
                ClaimOutcome::Claimed(SlotLease {
                    index: i,
                    file: path,
                    tenant,
                    task_id: task_id.to_string(),
                })
            }
            Err(_) => ClaimOutcome::Reclaimed,
        }
    }

    /// Whether an alien (live) tenant holds any slot.
    fn alien_tenant_active(&self) -> bool {
        let me = std::process::id() as u32;
        for i in 0..self.inner.max_slots {
            if let Some(l) = self.read_lease(i)
                && !self.lease_reclaimable(Some(&l))
                && l.tenant != me
            {
                return true;
            }
        }
        false
    }

    /// Acquire a slot lease, waiting (FIFO) until one is available.
    pub async fn acquire(&self, task_id: &str) -> SlotLease {
        loop {
            // Fast path: no alien tenant, try to claim a free slot.
            if !self.alien_tenant_active() {
                for i in 0..self.inner.max_slots {
                    if let ClaimOutcome::Claimed(lease) = self.try_claim(i, task_id) {
                        return lease;
                    }
                }
            }

            // Slow path: enqueue in FIFO order and wait for a wake or poll.
            let (tx, rx) = oneshot::channel::<()>();
            let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
            {
                let mut w = self.inner.waiters.lock().unwrap_or_else(|e| e.into_inner());
                w.queue.push_back(id);
                w.channels.insert(id, tx);
            }

            // Wait for a wake (release) or the poll timeout.
            tokio::select! {
                _ = rx => {}
                _ = sleep(self.inner.poll_interval) => {}
            }

            // Remove our wait-queue entry (idempotent).
            {
                let mut w = self.inner.waiters.lock().unwrap_or_else(|e| e.into_inner());
                w.queue.retain(|&q| q != id);
                w.channels.remove(&id);
            }
            // Loop: re-check the fast path.
        }
    }

    /// Release a held slot lease. Idempotent: a double-release is a no-op.
    ///
    /// Only unlinks the lease if it is owned by this process or the owner is
    /// dead; a live foreign owner's lease is left intact.
    pub fn release(&self, lease: &SlotLease) -> ReleaseResult {
        let path = &lease.file;
        let me = std::process::id() as u32;
        let cur = self.read_lease(lease.index);

        let should_unlink = match &cur {
            None => true,
            // Ours, or the owner is dead (including unreadable): free it.
            Some(l) if l.tenant == me || !pid_alive(l.tenant) => true,
            // Another live tenant owns the lease; it was not deleted.
            Some(_) => false,
        };

        if !should_unlink {
            return ReleaseResult::NotOurs;
        }

        let _ = fs::remove_file(path);

        // Wake the head waiter (FIFO) so it can try to claim.
        let woken = {
            let mut w = self.inner.waiters.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(&head) = w.queue.front() {
                w.queue.pop_front();
                w.channels.remove(&head)
            } else {
                None
            }
        };
        if let Some(tx) = woken {
            let _ = tx.send(());
        }

        ReleaseResult::Released
    }

    /// List active (non-reclaimable) leases across all slots.
    pub fn list_slots(&self) -> Vec<Lease> {
        let mut out = Vec::new();
        for i in 0..self.inner.max_slots {
            if let Some(l) = self.read_lease(i)
                && !self.lease_reclaimable(Some(&l))
            {
                out.push(l);
            }
        }
        out
    }

    /// Deterministic slot status distinguishing same-tenant vs alien-tenant
    /// holders (for status reporting / wait advisories).
    pub fn slot_status(&self, current_tenant: u32) -> SlotStatus {
        let mut slots = Vec::new();
        let mut alien = Vec::new();
        let mut same = Vec::new();
        for i in 0..self.inner.max_slots {
            if let Some(l) = self.read_lease(i)
                && !self.lease_reclaimable(Some(&l))
            {
                let entry = SlotEntry {
                    slot: i,
                    lease: l.clone(),
                };
                if l.tenant == current_tenant {
                    same.push(entry.clone());
                } else {
                    alien.push(entry.clone());
                }
                slots.push(entry);
            }
        }
        let alien_active = !alien.is_empty();
        let same_active = !same.is_empty();
        let fully_occupied = slots.len() >= self.inner.max_slots;
        SlotStatus {
            slots,
            total_capacity: self.inner.max_slots,
            alien_holders: alien,
            same_holders: same,
            is_alien_active: alien_active,
            is_same_active: same_active,
            is_fully_occupied: fully_occupied,
        }
    }

    /// Clear reclaimable slot leases (dead owners or unreadable). Used at boot
    /// and after a crash.
    pub fn clear_reclaimable(&self) {
        let _ = fs::create_dir_all(&self.inner.slots_dir);
        let entries = match fs::read_dir(&self.inner.slots_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !name.starts_with("slot_") || !name.ends_with(".json") {
                continue;
            }
            let lease: Option<Lease> = fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok());
            if self.lease_reclaimable(lease.as_ref()) {
                let _ = fs::remove_file(&path);
            }
        }
    }
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

fn write_all(f: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    f.write_all(bytes)?;
    f.flush()
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
        let p = std::env::temp_dir().join(format!("castor-sem-{}-{}", std::process::id(), n));
        let s = StateDir::new(p);
        s.ensure().unwrap();
        s
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Path to the compiled castor binary (used for cross-process tests).
    ///
    /// In test mode `current_exe()` is `target/debug/deps/castor-<hash>`; the
    /// real binary is `target/debug/castor` (two levels up).
    fn castor_bin() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let debug_dir = exe
            .parent() // .../deps
            .and_then(|p| p.parent()) // .../debug
            .unwrap()
            .to_path_buf();
        let bin = debug_dir.join("castor");
        if cfg!(windows) {
            bin.with_extension("exe")
        } else {
            bin
        }
    }

    /// A defunct (zombie) worker must be treated as DEAD so it cannot hold a
    /// slot (e.g. `slot_0.json`) and deadlock other tenants.
    ///
    /// Spawns a trivial child, lets it exit, but does NOT reap it — the kernel
    /// keeps it as a `<defunct>` zombie. On Linux `kill(pid, 0)` still returns
    /// 0 for a zombie, so this exercises the `/proc/{pid}/status` `State: Z`
    /// re-inspection in [`pid_alive`].
    #[cfg(target_os = "linux")]
    #[test]
    fn zombie_pid_is_treated_dead() {
        // Deliberately do NOT call .wait() before the probe: an unreaped,
        // exited child stays a zombie.
        let mut child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        assert!(pid > 0);

        // Poll until /proc/{pid}/status reports State: Z (i.e. it has exited
        // and become a zombie). This is an independent confirmation that the
        // process is a zombie, so the assertion below truly tests pid_alive.
        let status_path = format!("/proc/{pid}/status");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let is_zombie = std::fs::read_to_string(&status_path)
                .ok()
                .and_then(|c| {
                    c.lines()
                        .find(|l| l.starts_with("State:"))
                        .map(|l| l.trim_start_matches("State:").trim_start().starts_with('Z'))
                })
                .unwrap_or(false);
            if is_zombie {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("child {pid} did not become a zombie within 5s");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // The fix: a zombie must be reported as not alive.
        assert!(
            !pid_alive(pid),
            "zombie pid {pid} must be treated as dead (it cannot release its slot)"
        );

        // Reap so nothing is left around; after reaping the pid is gone.
        let _ = child.wait();
        assert!(!pid_alive(pid), "reaped pid {pid} must be dead");
    }

    #[test]
    fn first_acquire_is_immediate_and_single_lease() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 1);
        let lease = rt().block_on(sem.acquire("t1"));
        assert_eq!(sem.list_slots().len(), 1);
        let r = sem.release(&lease);
        assert_eq!(r, ReleaseResult::Released);
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn second_acquire_blocks_until_release() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 1, 50);
        let a = rt().block_on(sem.acquire("a"));
        // Spawn a second acquire on the same runtime; it should block.
        let sem2 = sem.clone();
        let handle = rt().block_on(async move {
            let f = sem2.acquire("b");
            tokio::time::timeout(Duration::from_millis(200), f).await
        });
        // Should still be pending (timed out) because `a` is held.
        assert!(handle.is_err(), "second acquire should be blocked");
        // Now release `a`; the next acquire should succeed quickly.
        let _ = sem.release(&a);
        let b = rt().block_on(sem.acquire("b"));
        assert_eq!(b.task_id, "b");
        let _ = sem.release(&b);
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn double_release_is_idempotent() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 1);
        let lease = rt().block_on(sem.acquire("x"));
        let r1 = sem.release(&lease);
        assert_eq!(r1, ReleaseResult::Released);
        let r2 = sem.release(&lease);
        assert_eq!(r2, ReleaseResult::Released);
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn zero_leftover_leases_after_release() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 2);
        let a = rt().block_on(sem.acquire("a"));
        let b = rt().block_on(sem.acquire("b"));
        assert_eq!(sem.list_slots().len(), 2);
        let _ = sem.release(&a);
        let _ = sem.release(&b);
        let mut count = 0;
        for e in fs::read_dir(state.task_slots()).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("slot_") && n.ends_with(".json") {
                count += 1;
            }
        }
        assert_eq!(count, 0);
    }

    #[test]
    fn cross_process_slot_exclusion() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 1, 100);
        let bin = castor_bin();
        assert!(bin.exists(), "castor binary not found at {bin:?}");

        // Spawn a child that acquires a slot, prints HELD, holds 3s, releases.
        let mut child = std::process::Command::new(&bin)
            .args(["__sem_child"])
            .arg(state.root().to_str().unwrap())
            .arg("child_task")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // Wait for HELD.
        use std::io::BufRead;
        let mut line = String::new();
        {
            let stdout = child.stdout.take().unwrap();
            let mut reader = std::io::BufReader::new(stdout);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 {
                    panic!("child exited before printing HELD");
                }
                if line.contains("HELD") {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    panic!("timeout waiting for HELD");
                }
            }
        }

        // Parent should be blocked while child holds the slot.
        let sem2 = sem.clone();
        let blocked = rt().block_on(async move {
            let f = sem2.acquire("parent_task");
            tokio::time::timeout(Duration::from_millis(500), f).await
        });
        assert!(
            blocked.is_err(),
            "parent acquire should be blocked while child holds slot"
        );

        // Wait for child to exit (it releases after 3s).
        let status = child.wait().unwrap();
        assert!(status.success(), "child should exit cleanly");

        // Now parent should acquire.
        let lease = rt().block_on(sem.acquire("parent_task"));
        assert_eq!(lease.task_id, "parent_task");
        let _ = sem.release(&lease);
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn alien_tenant_blocked_then_unblocked() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 1, 100);
        let bin = castor_bin();
        assert!(bin.exists(), "castor binary not found at {bin:?}");

        // Child holds a slot for 2s.
        let mut child = std::process::Command::new(&bin)
            .args(["__sem_child"])
            .arg(state.root().to_str().unwrap())
            .arg("alien_task")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        use std::io::BufRead;
        let mut line = String::new();
        {
            let stdout = child.stdout.take().unwrap();
            let mut reader = std::io::BufReader::new(stdout);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let n = reader.read_line(&mut line).unwrap();
                if n == 0 {
                    panic!("child exited before HELD");
                }
                if line.contains("HELD") {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    panic!("timeout waiting for HELD");
                }
            }
        }

        // Parent is blocked (alien tenant active).
        let sem2 = sem.clone();
        let blocked = rt().block_on(async move {
            let f = sem2.acquire("parent");
            tokio::time::timeout(Duration::from_millis(500), f).await
        });
        assert!(blocked.is_err(), "parent should be blocked by alien tenant");

        // Wait for child to exit.
        let status = child.wait().unwrap();
        assert!(status.success());

        // Now parent can acquire.
        let lease = rt().block_on(sem.acquire("parent"));
        assert_eq!(lease.task_id, "parent");
        let _ = sem.release(&lease);
    }

    #[test]
    fn fifo_order() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 1, 50);
        let rt = rt();

        // Do everything inside a single block_on so spawned tasks have a
        // reactor context.
        let sem_inner = sem.clone();
        let (_first, second) = rt.block_on(async move {
            let sem1 = sem_inner.clone();
            let sem2 = sem_inner.clone();
            let h1 = tokio::spawn(async move { sem1.acquire("first").await });
            let h2 = tokio::spawn(async move { sem2.acquire("second").await });

            // First should complete quickly.
            let first = tokio::time::timeout(Duration::from_millis(500), h1)
                .await
                .expect("first acquire should complete")
                .unwrap();
            assert_eq!(first.task_id, "first");

            // Release first; second should now acquire.
            sem_inner.release(&first);
            let second = tokio::time::timeout(Duration::from_millis(2000), h2)
                .await
                .expect("second acquire should complete after first is released")
                .unwrap();
            (first, second)
        });

        assert_eq!(second.task_id, "second");
        let _ = sem.release(&second);
        assert_eq!(sem.list_slots().len(), 0);
    }
}
