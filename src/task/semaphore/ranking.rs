//! KV-fill priority ranking, soft pool limits, and admission logic.

use std::cmp::Ordering;

use super::lease::Lease;

/// Compare two leases by priority:
/// 1. Higher `context_tokens` is heavier, prioritized to finish and free its large memory.
/// 2. Older task (smaller timestamp `at`) is senior.
/// 3. Deterministic tie-breaker by `task_id`.
pub fn lease_rank(a: &Lease, b: &Lease) -> Ordering {
    // 1. Higher context_tokens is heavier, prioritized to finish and free its large memory
    if a.context_tokens != b.context_tokens {
        return a.context_tokens.cmp(&b.context_tokens);
    }
    // 2. Older task (smaller timestamp `at`) is senior
    if a.at != b.at {
        return b.at.cmp(&a.at);
    }
    // 3. Deterministic tie-breaker
    b.task_id.cmp(&a.task_id)
}

/// Soft limit for the pool's token budget (90% of capacity).
pub fn soft_pool_limit(pool_capacity_tokens: u32) -> u64 {
    (pool_capacity_tokens as u64 * 90) / 100
}

/// Total committed context tokens across a slice of active leases.
pub fn committed_tokens(leases: &[Lease]) -> u64 {
    leases.iter().map(|l| l.context_tokens as u64).sum()
}

/// Whether the aggregate KV pool is exhausted (committed >= capacity).
pub fn is_pool_exhausted(pool_capacity_tokens: u32, committed: u64) -> bool {
    if pool_capacity_tokens == 0 {
        return false;
    }
    committed >= pool_capacity_tokens as u64
}

/// DRY token-budget admission gate.
///
/// Halts new task admission if aggregate committed tokens exceed the soft limit,
/// or if the new task's initial tokens would push the pool over total capacity.
pub fn token_budget_admits(pool_capacity_tokens: u32, committed: u64, initial_tokens: u32) -> bool {
    if pool_capacity_tokens == 0 {
        return true; // token-budget gate disabled
    }
    let soft_limit = soft_pool_limit(pool_capacity_tokens);
    if committed > soft_limit {
        return false;
    }
    let new_total = committed + initial_tokens as u64;
    new_total <= pool_capacity_tokens as u64
}

/// Decide whether an active lease should yield (pause) to allow
/// a heavier or senior active task to finish and free its KV cache.
pub fn should_yield(
    slots: &[Lease],
    task_id: &str,
    tenant: u32,
    pool_capacity_tokens: u32,
) -> bool {
    if slots.len() <= 1 {
        // Solo worker: no one to yield to, and no contention.
        return false;
    }
    if pool_capacity_tokens == 0 {
        return false;
    }
    let committed = committed_tokens(slots);
    if committed <= soft_pool_limit(pool_capacity_tokens) {
        return false;
    }

    let our_lease = slots
        .iter()
        .find(|l| l.task_id == task_id && l.tenant == tenant);
    let Some(ours) = our_lease else {
        return false;
    };

    // If there exists ANY other active lease with higher rank (heavier context or senior),
    // we yield (pause) so that task can finish and release its KV footprint.
    slots
        .iter()
        .any(|other| other.task_id != ours.task_id && lease_rank(other, ours) == Ordering::Greater)
}
