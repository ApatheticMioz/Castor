//! Cross-process task-slot semaphore.
//!
//! Slots are file leases at `<state>/tasks/slots/slot_N.json` (N in `0..max_slots`).
//! A lease is claimed with an atomic O_EXCL create and freed by an ownership-checked
//! unlink. Stale lease reclamation is based on PID liveness (`is_process_alive`)
//! and zombie detection.
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
    /// Total token budget for the pool. `0` means the token-budget check is
    /// disabled (original slot-count-only semantics).
    pool_capacity_tokens: u32,
    /// Static token overhead charged per slot state entry.
    per_slot_state_tokens: u32,
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
                pool_capacity_tokens: 0,
                per_slot_state_tokens: 0,
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
                pool_capacity_tokens: 0,
                per_slot_state_tokens: 0,
            }),
        }
    }

    /// Set the token-budget parameters on a [`TaskSemaphore`] (builder pattern).
    ///
    /// - `pool_capacity_tokens`: total token budget for the whole pool. When
    ///   `0`, the token-budget gate is disabled (original slot-count-only
    ///   semantics).
    /// - `per_slot_state_tokens`: static token overhead charged per slot state
    ///   entry (the metadata / bookkeeping cost of each lease record).
    ///
    /// The combined per-lease cost model is:
    /// `per_slot_state_tokens + lease.context_tokens`
    ///
    /// See [`Self::acquire_with_tokens`] for the claim-time gate.
    pub fn with_token_budget(
        mut self,
        pool_capacity_tokens: u32,
        per_slot_state_tokens: u32,
    ) -> Self {
        // `inner` is an `Arc<Inner>`; to mutate the fields we need `Arc::get_mut`
        // (which panics if there are other strong refs). In practice this is
        // called immediately after construction (builder pattern), so there is
        // only one strong ref.
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_token_budget must be called before any clone is made");
        inner.pool_capacity_tokens = pool_capacity_tokens;
        inner.per_slot_state_tokens = per_slot_state_tokens;
        self
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

    /// Try to claim a single slot index with a given initial context-token
    /// value baked into the persisted [`Lease`].
    fn try_claim(&self, i: usize, task_id: &str, context_tokens: u32) -> ClaimOutcome {
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
                    context_tokens,
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
                    context_tokens,
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
    ///
    /// When `max_slots > 1`, multiple concurrent workers are permitted across
    /// distinct slot files (e.g. slots 0..3 for multi-agent execution); each slot
    /// is independently protected by O_EXCL file creation. Exclusivity across alien
    /// tenants applies only in single-slot (`max_slots == 1`) environments.
    fn alien_tenant_active(&self) -> bool {
        if self.inner.max_slots > 1 {
            return false;
        }
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
    ///
    /// This is the token-budget-free variant; equivalent to
    /// [`Self::acquire_with_tokens`] with `initial_tokens = 0` and no
    /// token-budget enforcement.
    pub async fn acquire(&self, task_id: &str) -> SlotLease {
        self.acquire_with_tokens(task_id, 0).await
    }

    /// Acquire a slot lease with an explicit initial context-token value,
    /// enforcing the token-budget constraint when one is configured.
    ///
    /// When `pool_capacity_tokens > 0` and at least one live lease exists,
    /// the claim is refused (and the caller re-queues / polls) unless:
    ///
    /// ```text
    ///   sum over all live leases L of (per_slot_state_tokens + L.context_tokens)
    ///   + per_slot_state_tokens + initial_tokens
    ///   <= pool_capacity_tokens
    /// ```
    ///
    /// When `pool_capacity_tokens == 0` the token-budget check is bypassed
    /// entirely (original slot-count-only semantics).
    pub async fn acquire_with_tokens(&self, task_id: &str, initial_tokens: u32) -> SlotLease {
        loop {
            // Fast path: no alien tenant, try to claim a free slot.
            if !self.alien_tenant_active() {
                for i in 0..self.inner.max_slots {
                    // Token-budget gate: when a budget is configured and there
                    // is at least one live lease, the new claim must not push
                    // the committed total over the pool capacity.
                    if self.inner.pool_capacity_tokens > 0 {
                        let live = self.list_slots();
                        if !live.is_empty() {
                            let committed: u64 = live
                                .iter()
                                .map(|l| {
                                    (self.inner.per_slot_state_tokens as u64)
                                        + (l.context_tokens as u64)
                                })
                                .sum();
                            let new_total = committed
                                + (self.inner.per_slot_state_tokens as u64)
                                + (initial_tokens as u64);
                            if new_total > (self.inner.pool_capacity_tokens as u64) {
                                // Budget exceeded — skip this slot for now.
                                continue;
                            }
                        }
                    }
                    if let ClaimOutcome::Claimed(lease) = self.try_claim(i, task_id, initial_tokens)
                    {
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

    /// Update the `context_tokens` field of an existing live lease on disk.
    ///
    /// This is a read-modify-write on the lease file; it is safe for
    /// single-tenant-per-slot use. If the lease is absent or not owned by
    /// this process the update is silently ignored (no-op).
    pub fn update_context_tokens(&self, task_id: &str, context_tokens: u32) {
        let me = std::process::id() as u32;
        for i in 0..self.inner.max_slots {
            let Some(lease) = self.read_lease(i) else {
                continue;
            };
            if lease.tenant != me {
                continue;
            }
            if !lease.task_id.is_empty() && lease.task_id != task_id {
                continue;
            }
            let updated = Lease {
                context_tokens,
                ..lease
            };
            let path = self.slot_path(i);
            if let Ok(json) = serde_json::to_string(&updated) {
                let _ = fs::write(&path, json);
            }
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

    /// Decide whether a held [`SlotLease`] should yield its slot back to the
    /// pool so a newer / more cache-efficient worker can proceed.
    ///
    /// This is the DRY core of the slot-yield gate (Issues #23 / #28).
    /// `cache_hit_yield_ratio` and `min_eviction_tokens` are injected by the
    /// caller from [`crate::config::Config`] so this method stays free of any
    /// config dependency (and trivially unit-testable).
    ///
    /// Rules:
    /// - A solo worker (`slots.len() <= 1`) never yields.
    /// - The **oldest** active lease has right-of-way to finish and never
    ///   yields (right-of-way by acquisition timestamp, tie-broken by
    ///   `task_id`).
    /// - Otherwise a younger worker yields when **either** condition holds:
    ///   - **A (empirical cache eviction)**: `cached`/`prompt` are both known,
    ///     `prompt >= min_eviction_tokens` (> 0), and the cache hit ratio
    ///     `cached / prompt < cache_hit_yield_ratio`.
    ///   - **B (physical pool overcommit)**: the pool token budget is enabled
    ///     (`pool_capacity_tokens > 0`) and the committed total
    ///     `sum(per_slot_state_tokens + lease.context_tokens)` exceeds it.
    pub fn should_yield(
        &self,
        lease: &SlotLease,
        _context_tokens: u32,
        cached_tokens: Option<u32>,
        prompt_tokens: Option<u32>,
        min_eviction_tokens: u32,
        cache_hit_yield_ratio: f64,
    ) -> bool {
        let slots = self.list_slots();
        if slots.len() <= 1 {
            // Solo worker: no one to yield to, and no contention.
            return false;
        }

        // This lease's acquisition timestamp (from its persisted record).
        let my_at = self
            .read_lease(lease.index)
            .map(|l| l.at)
            .unwrap_or(u64::MAX);

        // Right-of-way: yield only if at least one other active lease is
        // *older* than this one. If this is the oldest, it gets to finish.
        let is_younger = slots.iter().any(|other| {
            other.task_id != lease.task_id
                && (other.at < my_at || (other.at == my_at && other.task_id < lease.task_id))
        });
        if !is_younger {
            return false;
        }

        // Condition A: empirical cache eviction.
        if let (Some(cached), Some(prompt)) = (cached_tokens, prompt_tokens)
            && prompt >= min_eviction_tokens
            && min_eviction_tokens > 0
        {
            let hit_ratio = (cached as f64) / (prompt as f64);
            if hit_ratio < cache_hit_yield_ratio {
                return true;
            }
        }

        // Condition B: physical token pool overcommit.
        if self.inner.pool_capacity_tokens > 0 {
            let committed: u32 = slots
                .iter()
                .map(|l| {
                    self.inner
                        .per_slot_state_tokens
                        .saturating_add(l.context_tokens)
                })
                .sum();
            if committed > self.inner.pool_capacity_tokens {
                return true;
            }
        }

        false
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

    #[test]
    fn test_concurrency_4_agents_acquire_distinct_slots() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 4);
        let rt = rt();

        let leases = rt.block_on(async {
            let mut handles = Vec::new();
            for i in 0..4 {
                let s = sem.clone();
                let task_id = format!("agent_{i}");
                handles.push(tokio::spawn(async move { s.acquire(&task_id).await }));
            }
            let mut out = Vec::new();
            for h in handles {
                out.push(
                    tokio::time::timeout(Duration::from_millis(1000), h)
                        .await
                        .unwrap()
                        .unwrap(),
                );
            }
            out
        });

        assert_eq!(leases.len(), 4);
        let mut slot_indices: Vec<usize> = leases.iter().map(|l| l.index).collect();
        slot_indices.sort_unstable();
        assert_eq!(slot_indices, vec![0, 1, 2, 3]);

        assert_eq!(sem.list_slots().len(), 4);
        let status = sem.slot_status(std::process::id() as u32);
        assert!(status.is_fully_occupied);
        assert_eq!(status.total_capacity, 4);
        assert_eq!(status.slots.len(), 4);

        for l in &leases {
            assert_eq!(sem.release(l), ReleaseResult::Released);
        }
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn test_concurrency_3_agents_in_4_slots_leaves_headroom() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 4);
        let rt = rt();

        let leases = rt.block_on(async {
            let mut out = Vec::new();
            for i in 0..3 {
                let task_id = format!("agent_{i}");
                out.push(sem.acquire(&task_id).await);
            }
            out
        });

        assert_eq!(leases.len(), 3);
        assert_eq!(sem.list_slots().len(), 3);
        let status = sem.slot_status(std::process::id() as u32);
        assert!(!status.is_fully_occupied);
        assert_eq!(status.total_capacity, 4);

        // Immediate 4th acquire succeeds without blocking
        let agent4 = rt.block_on(async {
            tokio::time::timeout(Duration::from_millis(200), sem.acquire("agent_3"))
                .await
                .expect("4th agent should acquire immediately")
        });
        assert_eq!(sem.list_slots().len(), 4);

        // Release slot 1 (agent_1)
        assert_eq!(sem.release(&leases[1]), ReleaseResult::Released);
        assert_eq!(sem.list_slots().len(), 3);

        // 5th acquire should immediately reclaim slot 1
        let agent5 = rt.block_on(async {
            tokio::time::timeout(Duration::from_millis(200), sem.acquire("agent_4"))
                .await
                .expect("5th agent should claim freed slot immediately")
        });
        assert_eq!(agent5.index, leases[1].index);
        assert_eq!(sem.list_slots().len(), 4);

        // Clean up
        sem.release(&leases[0]);
        sem.release(&leases[2]);
        sem.release(&agent4);
        sem.release(&agent5);
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn test_concurrency_10_agents_4_slots_bounded_fifo() {
        use std::sync::atomic::{AtomicUsize, Ordering as Ord};

        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 4, 20);
        let rt = rt();

        let active_count = Arc::new(AtomicUsize::new(0));
        let max_observed = Arc::new(AtomicUsize::new(0));
        let total_completed = Arc::new(AtomicUsize::new(0));

        rt.block_on(async {
            let mut handles = Vec::new();
            for i in 0..10 {
                let s = sem.clone();
                let active = active_count.clone();
                let max_obs = max_observed.clone();
                let done = total_completed.clone();
                let task_id = format!("worker_{i}");

                handles.push(tokio::spawn(async move {
                    let lease = s.acquire(&task_id).await;
                    let cur = active.fetch_add(1, Ord::SeqCst) + 1;
                    max_obs.fetch_max(cur, Ord::SeqCst);

                    // Concurrency invariant: active slots MUST never exceed capacity of 4
                    assert!(cur <= 4, "active count {cur} exceeded max_slots 4!");

                    // Simulate work (15ms)
                    tokio::time::sleep(Duration::from_millis(15)).await;

                    active.fetch_sub(1, Ord::SeqCst);
                    let res = s.release(&lease);
                    assert_eq!(res, ReleaseResult::Released);
                    done.fetch_add(1, Ord::SeqCst);
                }));
            }

            for h in handles {
                tokio::time::timeout(Duration::from_secs(10), h)
                    .await
                    .expect("task timed out")
                    .unwrap();
            }
        });

        assert_eq!(total_completed.load(Ord::SeqCst), 10);
        assert_eq!(max_observed.load(Ord::SeqCst), 4);
        assert_eq!(active_count.load(Ord::SeqCst), 0);
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn test_multi_process_4_slots_alien_exclusion() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 4, 50);
        let bin = castor_bin();
        assert!(bin.exists(), "castor binary not found at {bin:?}");

        // Spawn 4 separate child processes each claiming 1 slot and holding for 2 seconds
        let mut children = Vec::new();
        for i in 0..4 {
            let child = std::process::Command::new(&bin)
                .args(["__sem_child"])
                .arg(state.root().to_str().unwrap())
                .arg(format!("child_task_{i}"))
                .arg("--max-slots")
                .arg("4")
                .arg("--hold-secs")
                .arg("10")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            children.push(child);
        }

        use std::io::BufRead;
        // Wait for all 4 children to output "HELD"
        for child in &mut children {
            let stdout = child.stdout.take().unwrap();
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
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

        // Now all 4 slots are occupied by alien processes!
        assert_eq!(sem.list_slots().len(), 4);

        // A 5th acquire from the parent process MUST block!
        let sem_clone = sem.clone();
        let blocked = rt().block_on(async move {
            let f = sem_clone.acquire("parent_task_5th");
            tokio::time::timeout(Duration::from_millis(300), f).await
        });
        assert!(
            blocked.is_err(),
            "5th acquire must be blocked when 4 slots are occupied"
        );

        // Wait for children to finish (they hold for ~3s and exit)
        for mut child in children {
            let status = child.wait().unwrap();
            assert!(status.success());
        }

        // Now parent should be able to acquire immediately
        let lease = rt().block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                sem.acquire("parent_task_after_drain"),
            )
            .await
            .expect("parent acquire should succeed after drain")
        });
        assert_eq!(lease.task_id, "parent_task_after_drain");
        assert_eq!(sem.release(&lease), ReleaseResult::Released);
        assert_eq!(sem.list_slots().len(), 0);
    }

    #[test]
    fn test_multi_slot_crashed_tenant_reclaim() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 4);

        // Artificially inject a dead lease at slot 1 and slot 3 (PID 99999999)
        let dead_lease_1 = Lease {
            tenant: 99999999,
            task_id: "crashed_1".into(),
            at: 1000,
            hb: 1000,
            context_tokens: 0,
        };
        let dead_lease_3 = Lease {
            tenant: 99999999,
            task_id: "crashed_3".into(),
            at: 1000,
            hb: 1000,
            context_tokens: 0,
        };
        fs::write(
            sem.slot_path(1),
            serde_json::to_string(&dead_lease_1).unwrap(),
        )
        .unwrap();
        fs::write(
            sem.slot_path(3),
            serde_json::to_string(&dead_lease_3).unwrap(),
        )
        .unwrap();

        // Clear reclaimable slots
        sem.clear_reclaimable();
        assert_eq!(sem.list_slots().len(), 0);

        // Now acquire all 4 slots cleanly
        let l0 = rt().block_on(sem.acquire("t0"));
        let l1 = rt().block_on(sem.acquire("t1"));
        let l2 = rt().block_on(sem.acquire("t2"));
        let l3 = rt().block_on(sem.acquire("t3"));
        assert_eq!(sem.list_slots().len(), 4);

        sem.release(&l0);
        sem.release(&l1);
        sem.release(&l2);
        sem.release(&l3);
        assert_eq!(sem.list_slots().len(), 0);
    }

    // ------------------------------------------------------------------
    // Token-budget tests
    // ------------------------------------------------------------------

    /// A lease acquired via the plain `acquire` path must have
    /// `context_tokens == 0` (default).
    #[test]
    fn context_tokens_default_zero_on_acquire() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 1);
        let sem_c = sem.clone();
        let lease = rt().block_on(async move { sem_c.acquire("t1").await });
        let on_disk = sem.read_lease(lease.index).unwrap();
        assert_eq!(on_disk.context_tokens, 0);
        let _ = sem.release(&lease);
    }

    /// A `Lease` JSON written before `context_tokens` existed must
    /// deserialize with `context_tokens == 0` (backward compat).
    #[test]
    fn context_tokens_backward_compat_deserialization() {
        let json = r#"{"tenant":123,"task_id":"t","at":1000,"hb":1000}"#;
        let lease: Lease = serde_json::from_str(json).unwrap();
        assert_eq!(lease.context_tokens, 0);
        assert_eq!(lease.task_id, "t");
    }

    /// `update_context_tokens` must persist the new value to disk.
    #[test]
    fn update_context_tokens_persists_to_disk() {
        let state = tmp_state();
        let sem = TaskSemaphore::new(&state, 2);
        let sem_c = sem.clone();
        let lease = rt().block_on(async move { sem_c.acquire("task_a").await });
        assert_eq!(sem.read_lease(lease.index).unwrap().context_tokens, 0);

        sem.update_context_tokens("task_a", 42);
        let updated = sem.read_lease(lease.index).unwrap();
        assert_eq!(updated.context_tokens, 42);

        // A non-matching task_id must NOT modify this lease.
        sem.update_context_tokens("other_task", 99);
        let unchanged = sem.read_lease(lease.index).unwrap();
        assert_eq!(
            unchanged.context_tokens, 42,
            "non-matching task_id must not overwrite"
        );

        let _ = sem.release(&lease);
    }

    /// With `pool_capacity_tokens = 100` and `per_slot_state_tokens = 10`:
    /// - First acquire (no live lease) → gate bypassed, claim succeeds.
    /// - Second acquire with `initial_tokens = 50`:
    ///   committed = 10 + 50 = 60; new_total = 60 + 10 + 50 = 120 > 100
    ///   → must be BLOCKED (times out).
    #[test]
    fn with_token_budget_blocks_when_over_capacity() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 4, 50).with_token_budget(100, 10);
        let rt = rt();

        // First acquire: no live leases → gate bypassed.
        let sem_c1 = sem.clone();
        let l1 = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                sem_c1.acquire_with_tokens("task_a", 50),
            )
            .await
            .expect("first acquire should succeed (no live leases yet)")
        });
        assert_eq!(sem.list_slots().len(), 1);

        // Second acquire: 1 live lease (context_tokens=50).
        // committed = 10 + 50 = 60
        // new_total = 60 + 10 + 50 = 120 > 100 → BLOCKED
        let sem_c2 = sem.clone();
        let blocked = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(200),
                sem_c2.acquire_with_tokens("task_b", 50),
            )
            .await
        });
        assert!(
            blocked.is_err(),
            "second acquire must be blocked: 120 > 100 (pool_capacity)"
        );

        let _ = sem.release(&l1);
        assert_eq!(sem.list_slots().len(), 0);
    }

    /// With `pool_capacity_tokens = 100` and `per_slot_state_tokens = 10`:
    /// - First acquire with `initial_tokens = 50` → 60 committed.
    /// - Second acquire with `initial_tokens = 20`:
    ///   committed = 10 + 50 = 60; new_total = 60 + 10 + 20 = 90 <= 100
    ///   → must SUCCEED.
    #[test]
    fn with_token_budget_allows_when_under_capacity() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 4, 50).with_token_budget(100, 10);
        let rt = rt();

        let sem_c1 = sem.clone();
        let l1 = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                sem_c1.acquire_with_tokens("task_a", 50),
            )
            .await
            .expect("first acquire should succeed")
        });
        assert_eq!(sem.list_slots().len(), 1);

        // 60 committed + 10 (per_slot) + 20 (initial) = 90 <= 100 → OK
        let sem_c2 = sem.clone();
        let l2 = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                sem_c2.acquire_with_tokens("task_b", 20),
            )
            .await
            .expect("second acquire should succeed: 90 <= 100")
        });
        assert_eq!(sem.list_slots().len(), 2);

        // Verify the persisted context_tokens values.
        let slot_a = sem.read_lease(l1.index).unwrap();
        let slot_b = sem.read_lease(l2.index).unwrap();
        assert_eq!(slot_a.context_tokens, 50);
        assert_eq!(slot_b.context_tokens, 20);

        sem.release(&l1);
        sem.release(&l2);
        assert_eq!(sem.list_slots().len(), 0);
    }

    /// When `pool_capacity_tokens == 0` (default), the token-budget gate is
    /// disabled and `acquire_with_tokens` behaves identically to `acquire`.
    #[test]
    fn zero_pool_capacity_disables_token_gate() {
        let state = tmp_state();
        // Default: pool_capacity_tokens = 0 → gate disabled.
        let sem = TaskSemaphore::with_poll_interval(&state, 4, 50);
        let rt = rt();

        // Acquire with a large initial_tokens value — should succeed
        // because the gate is disabled (pool_capacity == 0).
        let sem_c1 = sem.clone();
        let l1 = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                sem_c1.acquire_with_tokens("task_a", 999_999),
            )
            .await
            .expect("should succeed when gate is disabled")
        });
        let sem_c2 = sem.clone();
        let l2 = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                sem_c2.acquire_with_tokens("task_b", 999_999),
            )
            .await
            .expect("second should also succeed when gate is disabled")
        });
        assert_eq!(sem.list_slots().len(), 2);

        sem.release(&l1);
        sem.release(&l2);
    }

    /// `update_context_tokens` followed by a second acquire must reflect the
    /// updated token cost in the budget gate.
    #[test]
    fn update_context_tokens_affects_budget_gate() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 4, 50).with_token_budget(100, 10);
        let rt = rt();

        // First acquire with 20 initial tokens → committed = 10+20 = 30.
        let sem_c1 = sem.clone();
        let l1 = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(500),
                sem_c1.acquire_with_tokens("task_a", 20),
            )
            .await
            .expect("first acquire")
        });

        // Bump context_tokens to 80 → committed = 10+80 = 90.
        sem.update_context_tokens("task_a", 80);

        // Second acquire with 20 initial tokens:
        // new_total = 90 + 10 + 20 = 120 > 100 → BLOCKED.
        let sem_c2 = sem.clone();
        let blocked = rt.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(200),
                sem_c2.acquire_with_tokens("task_b", 20),
            )
            .await
        });
        assert!(
            blocked.is_err(),
            "second acquire must be blocked after context_tokens update: 120 > 100"
        );

        let _ = sem.release(&l1);
    }

    // ------------------------------------------------------------------
    // `should_yield` (slot-yield gate, Issues #23 / #28)
    // ------------------------------------------------------------------

    /// A solo worker (only one active lease) never yields.
    #[test]
    fn should_yield_solo_never_yields() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 1, 50);
        let a = rt().block_on(sem.acquire_with_tokens("a", 0));
        // Even with a pathological cache ratio, a solo worker keeps its slot.
        assert!(!sem.should_yield(&a, 0, Some(0), Some(1000), 100, 0.20));
    }

    /// The oldest active lease has right-of-way and never yields, even when
    /// its cache ratio is poor.
    #[test]
    fn should_yield_oldest_never_yields() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 2, 50);
        let a = rt().block_on(sem.acquire_with_tokens("a", 0)); // oldest
        let _b = rt().block_on(sem.acquire_with_tokens("b", 0));
        // `a` is the oldest active lease → right-of-way → no yield.
        assert!(!sem.should_yield(&a, 0, Some(0), Some(1000), 100, 0.20));
    }

    /// A younger lease with a low empirical cache-hit ratio yields (Condition A).
    #[test]
    fn should_yield_younger_low_cache_hit_ratio_yields() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 2, 50);
        let _a = rt().block_on(sem.acquire_with_tokens("a", 0));
        let b = rt().block_on(sem.acquire_with_tokens("b", 0)); // younger
        // pool disabled (no with_token_budget) → Condition A drives the result.
        // hit_ratio = 100/1000 = 0.10 < 0.20 → yield.
        assert!(sem.should_yield(&b, 0, Some(100), Some(1000), 100, 0.20));
    }

    /// A younger lease with a healthy cache-hit ratio and no pool overcommit
    /// does NOT yield.
    #[test]
    fn should_yield_younger_healthy_cache_does_not_yield() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 2, 50);
        let _a = rt().block_on(sem.acquire_with_tokens("a", 0));
        let b = rt().block_on(sem.acquire_with_tokens("b", 0)); // younger
        // hit_ratio = 500/1000 = 0.50 >= 0.20 → no Condition A;
        // pool disabled → no Condition B → no yield.
        assert!(!sem.should_yield(&b, 0, Some(500), Some(1000), 100, 0.20));
    }

    /// A younger lease that overcommits the physical token pool yields
    /// (Condition B).
    #[test]
    fn should_yield_younger_pool_overcommit_yields() {
        let state = tmp_state();
        let sem = TaskSemaphore::with_poll_interval(&state, 2, 50).with_token_budget(50, 10);
        let _a = rt().block_on(sem.acquire_with_tokens("a", 0));
        let b = rt().block_on(sem.acquire_with_tokens("b", 0)); // younger
        // Grow `b`'s context so committed = (10+0) + (10+100) = 120 > 50.
        sem.update_context_tokens("b", 100);
        // No cached/prompt → Condition A skipped; Condition B (120 > 50) → yield.
        assert!(sem.should_yield(&b, 0, None, None, 0, 0.20));
    }
}
