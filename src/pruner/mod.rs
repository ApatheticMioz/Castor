//! State directory retention / pruning.
//!
//! Retention policy semantics:
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

pub mod cleanup;
#[cfg(test)]
mod tests;

pub use cleanup::{
    maybe_auto_prune, needs_auto_prune, plan_scratch, prune_scratch_dir, record_last_prune,
};

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Default TTL for scratch directory artifacts (4 days in milliseconds).
pub const DEFAULT_SCRATCH_TTL_MS: u64 = 4 * 86_400_000;

/// State file recording epoch milliseconds of the last successful prune.
pub const LAST_PRUNE_FILE: &str = ".last_prune";

/// Background auto-pruning interval (24 hours in milliseconds).
pub const AUTO_PRUNE_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// Retention policy.
#[derive(Debug, Clone)]
pub struct PrunePolicy {
    /// Max age in milliseconds (default 14 days).
    pub max_age_ms: u64,
    /// Max number of sessions to keep (default 200).
    pub max_count: usize,
    /// Max total session size in MB (default 50).
    pub max_size_mb: u64,
    /// Max scratch artifact age in milliseconds (default 4 days).
    pub scratch_ttl_ms: u64,
    /// Override "now" (epoch ms) for deterministic tests.
    pub now: Option<u64>,
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self {
            max_age_ms: 14 * 86_400_000,
            max_count: 200,
            max_size_mb: 50,
            scratch_ttl_ms: DEFAULT_SCRATCH_TTL_MS,
            now: None,
        }
    }
}

impl PrunePolicy {
    fn now(&self) -> u64 {
        self.now.unwrap_or_else(cleanup::now_millis)
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
    /// Scratch directories or files to remove.
    pub scratch: Vec<PathBuf>,
    /// Stale or expired lock tombstones to remove.
    pub tombstones: Vec<PathBuf>,
    /// Set to `true` by [`apply`] once the plan has been executed.
    pub applied: bool,
}

impl PrunePlan {
    /// Total number of paths this plan would delete.
    pub fn len(&self) -> usize {
        self.sessions.len()
            + self.tasks.len()
            + self.telemetry.len()
            + self.evo.len()
            + self.scratch.len()
            + self.tombstones.len()
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
/// the sessions / tasks / telemetry / evo / scratch entries that would be deleted.
pub fn plan(state_dir: &Path, policy: &PrunePolicy) -> PrunePlan {
    let now = policy.now();

    let sessions = cleanup::plan_sessions(state_dir, policy, now);
    let tasks = cleanup::plan_tasks(state_dir, policy, now);
    let telemetry = cleanup::plan_telemetry(state_dir, policy, now);
    let evo = cleanup::plan_evo(state_dir, policy, now);
    let scratch = cleanup::plan_scratch(&state_dir.join(".scratch"), policy.scratch_ttl_ms, now);
    let tombstones = cleanup::plan_tombstones(state_dir, policy, now);

    let fingerprint = cleanup::compute_fingerprint(
        state_dir,
        &sessions,
        &tasks,
        &telemetry,
        &evo,
        &scratch,
        &tombstones,
    );

    PrunePlan {
        state_dir: state_dir.to_path_buf(),
        fingerprint,
        sessions,
        tasks,
        telemetry,
        evo,
        scratch,
        tombstones,
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
/// tasks/telemetry. For scratch items, directories are recursively deleted
/// and files are removed. I/O errors are returned verbatim.
pub fn apply(state_dir: &Path, plan: &mut PrunePlan) -> Result<usize, PruneError> {
    if plan.applied {
        return Err(PruneError::AlreadyApplied);
    }
    if plan.state_dir != state_dir.to_path_buf() {
        return Err(PruneError::NotFromPlan);
    }
    let fp = cleanup::compute_fingerprint(
        state_dir,
        &plan.sessions,
        &plan.tasks,
        &plan.telemetry,
        &plan.evo,
        &plan.scratch,
        &plan.tombstones,
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
    for p in &plan.scratch {
        if p.is_dir() {
            fs::remove_dir_all(p)?;
        } else {
            fs::remove_file(p)?;
        }
        deleted += 1;
    }
    for p in &plan.tombstones {
        fs::remove_file(p)?;
        deleted += 1;
    }

    plan.applied = true;
    Ok(deleted)
}
