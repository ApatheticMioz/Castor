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

pub mod lease;
pub mod ranking;
#[cfg(test)]
mod tests;

pub use lease::{
    ClaimOutcome, Lease, ReleaseResult, SlotEntry, SlotLease, SlotStatus, now_ms, pid_alive,
};
pub use ranking::lease_rank;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::sleep;

use crate::state::StateDir;

/// Default poll cadence (ms) when waiting to acquire a slot.
const DEFAULT_POLL_MS: u64 = 1000;

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
    /// Owning state dir (kept so callers can build a registry / read task
    /// records without re-deriving the path).
    state: StateDir,
    max_slots: usize,
    poll_interval: Duration,
    waiters: Mutex<Waiters>,
    next_id: AtomicU64,
    /// Total token budget for the pool. `0` means the token-budget check is
    /// disabled (original slot-count-only semantics).
    pool_capacity_tokens: u32,
}

impl TaskSemaphore {
    /// Build a semaphore over `state` with `max_slots` slots (clamped to >= 1)
    /// and the default 1s poll interval.
    pub fn new(state: &StateDir, max_slots: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots_dir: state.task_slots(),
                state: state.clone(),
                max_slots: max_slots.max(1),
                poll_interval: Duration::from_millis(DEFAULT_POLL_MS),
                waiters: Mutex::new(Waiters::default()),
                next_id: AtomicU64::new(0),
                pool_capacity_tokens: 0,
            }),
        }
    }

    /// Build with an explicit poll interval (used by tests to speed up the
    /// cross-process hand-off).
    pub fn with_poll_interval(state: &StateDir, max_slots: usize, poll_ms: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots_dir: state.task_slots(),
                state: state.clone(),
                max_slots: max_slots.max(1),
                poll_interval: Duration::from_millis(poll_ms.max(1)),
                waiters: Mutex::new(Waiters::default()),
                next_id: AtomicU64::new(0),
                pool_capacity_tokens: 0,
            }),
        }
    }

    /// Set the pool's token budget (builder pattern).
    ///
    /// - `pool_capacity_tokens`: total token budget for the whole pool. When
    ///   `0`, the token-budget gate is disabled (original slot-count-only
    ///   semantics).
    ///
    /// The unified admission rule (see [`Self::acquire_with_tokens`]) is:
    /// `sum over all live leases L of (L.context_tokens) + requested_tokens
    /// <= pool_capacity_tokens`. There is no separate per-slot state overhead —
    /// each lease's own `context_tokens` is its full cost.
    pub fn with_token_budget(mut self, pool_capacity_tokens: u32) -> Self {
        // `inner` is an `Arc<Inner>`; to mutate the fields we need `Arc::get_mut`
        // (which panics if there are other strong refs). In practice this is
        // called immediately after construction (builder pattern), so there is
        // only one strong ref.
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_token_budget must be called before any clone is made");
        inner.pool_capacity_tokens = pool_capacity_tokens;
        self
    }

    pub fn max_slots(&self) -> usize {
        self.inner.max_slots
    }

    pub fn slots_dir(&self) -> &PathBuf {
        &self.inner.slots_dir
    }

    /// Path of slot index `i` (`<state>/tasks/slots/slot_{i}.json`).
    pub fn slot_path(&self, i: usize) -> PathBuf {
        self.inner.slots_dir.join(format!("slot_{i}.json"))
    }

    /// Read a lease file; `None` if absent or unparseable.
    pub(crate) fn read_lease(&self, i: usize) -> Option<Lease> {
        lease::read_lease(&self.slot_path(i))
    }

    /// Resolve the state dir this semaphore's slots live under (so callers can
    /// build a [`TaskRegistry`] / read task records without re-plumbing state).
    pub fn state_dir(&self) -> StateDir {
        self.inner.state.clone()
    }

    /// Read the on-disk status of the task a lease is bound to.
    ///
    /// Returns `true` when the bound task has reached a **terminal** state
    /// (`completed` / `failed` / `cancelled`), `false` when it is active
    /// (`queued` / `executing`), and `None` when the bound task id is empty or
    /// the task record is absent / unparseable.
    pub fn bound_task_terminal(&self, lease: &Lease) -> Option<bool> {
        lease::bound_task_terminal(&self.inner.state, lease)
    }

    /// Whether a lease is reclaimable: unreadable (crashed mid-write) or owned
    /// by a dead pid. A live tenant's lease is NEVER reclaimable.
    fn lease_reclaimable(&self, lease: Option<&Lease>) -> bool {
        lease::lease_reclaimable(lease)
    }

    /// Full reclamation decision for a lease, combining owner-pid liveness
    /// with the bound task's terminal state.
    #[cfg(test)]
    pub(crate) fn lease_reclaimable_full(&self, lease: Option<&Lease>) -> bool {
        lease::lease_reclaimable_full(&self.inner.state, lease)
    }

    fn try_claim(&self, i: usize, task_id: &str, context_tokens: u32) -> ClaimOutcome {
        lease::try_claim(
            &self.inner.slots_dir,
            &self.slot_path(i),
            i,
            task_id,
            context_tokens,
        )
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

    /// Soft limit for the pool's token budget (90% of capacity).
    pub fn soft_pool_limit(&self) -> u64 {
        ranking::soft_pool_limit(self.inner.pool_capacity_tokens)
    }

    /// Aggregate committed context tokens across all active leases.
    pub fn committed_tokens(&self) -> u64 {
        ranking::committed_tokens(&self.list_slots())
    }

    /// Whether the aggregate KV pool is exhausted (committed >= capacity).
    pub fn is_pool_exhausted(&self) -> bool {
        ranking::is_pool_exhausted(self.inner.pool_capacity_tokens, self.committed_tokens())
    }

    /// DRY token-budget admission gate.
    ///
    /// Halts new task admission if aggregate committed tokens exceed the soft limit,
    /// or if the new task's initial tokens would push the pool over total capacity.
    fn token_budget_admits(&self, initial_tokens: u32) -> bool {
        ranking::token_budget_admits(
            self.inner.pool_capacity_tokens,
            self.committed_tokens(),
            initial_tokens,
        )
    }

    pub async fn acquire_with_tokens(&self, task_id: &str, initial_tokens: u32) -> SlotLease {
        loop {
            // Fast path: no alien tenant, and the token-budget gate admits
            // this claim (checked once over all live leases, so an empty pool
            // can never overcommit). If the gate refuses, we fall through to
            // the FIFO waiter path and re-check here on the next loop, so a
            // woken waiter can never overcommit either.
            if !self.alien_tenant_active() && self.token_budget_admits(initial_tokens) {
                for i in 0..self.inner.max_slots {
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
                // Write-then-rename so concurrent readers never observe a
                // truncated / 0-byte lease mid-write. The temp file is
                // same-directory so the rename is atomic on the same volume.
                let tmp =
                    self.inner
                        .slots_dir
                        .join(format!("slot_{}.{}_tmp", i, std::process::id()));
                if fs::write(&tmp, &json).is_ok() && fs::rename(&tmp, &path).is_err() {
                    // Rename collided (e.g. cross-device / busy) — fall back to
                    // a direct write and clean up the temp.
                    let _ = fs::write(&path, json);
                    let _ = fs::remove_file(&tmp);
                }
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

    /// Decide whether a held [`SlotLease`] should yield (pause) to allow
    /// a heavier or senior active task to finish and free its KV cache.
    pub fn should_yield(&self, lease: &SlotLease) -> bool {
        let slots = self.list_slots();
        ranking::should_yield(
            &slots,
            &lease.task_id,
            lease.tenant,
            self.inner.pool_capacity_tokens,
        )
    }

    /// Asynchronously wait (pause) before a turn while the pool is under pressure
    /// and another heavier session has higher priority.
    pub async fn wait_backpressure(&self, lease: &SlotLease) {
        while self.should_yield(lease) {
            tokio::time::sleep(self.inner.poll_interval).await;
        }
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

    /// **Compare-and-delete** for one slot file: re-read the file and delete
    /// it **only if** its `(tenant, task_id, at)` identity still matches the
    /// `expected` lease that was evaluated for reclamation (or the file is
    /// gone / unreadable). Returns `true` when a file was actually unlinked.
    #[cfg(test)]
    pub(crate) fn reap_if_match(&self, path: &Path, expected: Option<&Lease>) -> bool {
        lease::reap_if_match(path, expected)
    }

    /// Reap one slot file end-to-end: evaluate the lease under the full
    /// reclamation rule and, when reclaimable, delete it via the atomic
    /// compare-and-delete. Returns `true` when a file was unlinked.
    pub(crate) fn reap_slot_file(&self, path: &Path) -> bool {
        lease::reap_slot_file(&self.inner.state, path)
    }

    /// Clear reclaimable slot leases. Used at boot, after a crash, and after
    /// every task terminal transition.
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
            let _ = self.reap_slot_file(&path);
        }
    }

    /// Reap (delete) orphaned slot leases and return how many were removed.
    pub fn reap_slots(&self) -> usize {
        let _ = fs::create_dir_all(&self.inner.slots_dir);
        let entries = match fs::read_dir(&self.inner.slots_dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        let mut reaped = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !name.starts_with("slot_") || !name.ends_with(".json") {
                continue;
            }
            if self.reap_slot_file(&path) {
                reaped += 1;
            }
        }
        reaped
    }
}
