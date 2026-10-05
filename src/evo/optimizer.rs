//! Offline score-and-commit engine for the AVO lineage.
//!
//! Design invariants (locked):
//! - The AVO **variation operator is the dispatched agent** (external). This
//!   module never generates proposals; it only *scores* artifacts the agent
//!   produced and *records* them into the lineage.
//! - [`score_artifact`] runs every task dir under `evals_dir` through the
//!   real [`run_task`] and maps each [`Outcome`] to a fitness entry:
//!     - `Pass`  → `Some(1.0)`
//!     - `Fail`  → `Some(0.0)`
//!     - `Error` → `None` (excluded from the mean, reported with its reason)
//! - [`commit_artifact`] appends a lineage node whose fitness is the **mean
//!   over the non-Error entries** (i.e. Pass + Fail). If there are zero
//!   scoreable entries (all Error, or no tasks), the node's fitness is
//!   `None`. The node's parent is the current lineage head (or none for the
//!   first node).
//! - [`recommend`] is a thin alias for [`Lineage::best_parent`].
//!
//! `Error` entries are never averaged as 0: they are excluded from the mean
//! and reported verbatim, so a harness-internal failure can never silently
//! drag a candidate's fitness down.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::evals::runner::{Outcome, Variant, run_task};
use crate::evo::lineage::{CommitId, Lineage, LineageError, LineageNode};

/// One scored task within a [`ScoreReport`].
#[derive(Debug, Clone, PartialEq)]
pub struct FitnessEntry {
    /// The task id (from `task.toml`).
    pub task_id: String,
    /// The mapped fitness: `Some(1.0)` for Pass, `Some(0.0)` for Fail,
    /// `None` for Error.
    pub fitness: Option<f64>,
    /// The raw outcome, preserved for reporting (an `Error` carries its
    /// reason here).
    pub outcome: Outcome,
}

/// The report produced by [`score_artifact`]: one entry per task, in
/// directory order.
#[derive(Debug, Clone, Default)]
pub struct ScoreReport {
    pub entries: Vec<FitnessEntry>,
}

impl ScoreReport {
    /// The mean fitness over all **scoreable** (non-Error) entries, or
    /// `None` when there are no scoreable entries.
    ///
    /// `Error` entries are excluded (never averaged as 0).
    pub fn mean_fitness(&self) -> Option<f64> {
        let scoreable: Vec<f64> = self.entries.iter().filter_map(|e| e.fitness).collect();
        if scoreable.is_empty() {
            None
        } else {
            Some(scoreable.iter().sum::<f64>() / scoreable.len() as f64)
        }
    }

    /// The number of scoreable (non-Error) entries.
    pub fn scoreable_count(&self) -> usize {
        self.entries.iter().filter(|e| e.fitness.is_some()).count()
    }
}

/// Typed errors for the score-and-commit engine.
#[derive(Debug, Error)]
pub enum OptimizerError {
    /// The evals dir does not exist.
    #[error("evals dir not found: {0}")]
    EvalsDirMissing(String),
    /// The evals dir exists but contains no task subdirectories (a task dir
    /// is a subdirectory containing a `task.toml`).
    #[error("no task directories under {0}")]
    NoTasks(String),
    /// A lineage operation failed (unknown parent, duplicate id, ...).
    #[error("lineage: {0}")]
    Lineage(#[from] LineageError),
    /// I/O failure reading the evals dir.
    #[error("evals io: {0}")]
    Io(#[from] std::io::Error),
}

/// Score an artifact: run every task dir under `evals_dir` through the real
/// [`run_task`] for the given `variant` and collect one [`FitnessEntry`] per
/// task.
///
/// A missing evals dir yields [`OptimizerError::EvalsDirMissing`]; an
/// existing dir with no task subdirectories yields
/// [`OptimizerError::NoTasks`].
pub fn score_artifact(evals_dir: &Path, variant: Variant) -> Result<ScoreReport, OptimizerError> {
    if !evals_dir.is_dir() {
        return Err(OptimizerError::EvalsDirMissing(
            evals_dir.display().to_string(),
        ));
    }

    let mut task_dirs: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(evals_dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() && p.join("task.toml").is_file() {
            task_dirs.push(p);
        }
    }
    task_dirs.sort();

    if task_dirs.is_empty() {
        return Err(OptimizerError::NoTasks(evals_dir.display().to_string()));
    }

    let mut entries: Vec<FitnessEntry> = Vec::with_capacity(task_dirs.len());
    for dir in &task_dirs {
        let report = run_task(dir, variant);
        let fitness = match &report.outcome {
            Outcome::Pass => Some(1.0),
            Outcome::Fail(_) => Some(0.0),
            Outcome::Error(_) => None,
        };
        entries.push(FitnessEntry {
            task_id: report.task_id,
            fitness,
            outcome: report.outcome,
        });
    }

    Ok(ScoreReport { entries })
}

/// Commit a scored artifact into the lineage.
///
/// Appends a [`LineageNode`] whose:
/// - `id` is derived from `now_ms` (and the current node count, so it is
///   unique within the lineage),
/// - `parents` is `[current head]` (empty for the first node),
/// - `fitness` is the report's [`ScoreReport::mean_fitness`] (`None` when
///   there are no scoreable entries),
/// - `artifact_ref` is the caller-supplied reference,
/// - `created_at` is the ISO-8601 UTC rendering of `now_ms`.
///
/// Returns the new node's [`CommitId`].
pub fn commit_artifact(
    lineage: &mut Lineage,
    artifact_ref: String,
    report: &ScoreReport,
    now_ms: u64,
) -> Result<CommitId, OptimizerError> {
    let parents: Vec<CommitId> = match lineage.head() {
        Some(h) => vec![h.id.clone()],
        None => Vec::new(),
    };
    let id = format!("commit-{now_ms}-{}", lineage.nodes().len());
    let node = LineageNode {
        id: id.clone(),
        parents,
        fitness: report.mean_fitness(),
        artifact_ref,
        created_at: iso8601_utc(now_ms),
    };
    lineage.add_node(node)?;
    Ok(id)
}

/// The lineage's recommended parent for the next variation: the highest
/// fitness node (ties broken by the earliest `created_at`). Unscored nodes
/// are ineligible.
pub fn recommend(lineage: &Lineage) -> Option<&LineageNode> {
    lineage.best_parent()
}

/// Convert an epoch-millis timestamp to an ISO-8601 UTC string
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`). ISO-8601 strings compare
/// lexicographically, which is what [`Lineage::best_parent`] relies on for
/// tie-breaking.
pub fn iso8601_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Howard Hinnant's `civil_from_days` algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let mo = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    if mo <= 2 {
        y += 1;
    }

    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp dir for one test (cleaned up on drop).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("castor_evo_optimizer_{tag}_{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Write a minimal task dir (task.toml + a single-message golden trace +
    /// an empty fixture dir) that scores as `Pass` (no checks).
    fn write_pass_task(task_dir: &Path) {
        fs::create_dir_all(task_dir.join("fixture")).unwrap();
        fs::write(
            task_dir.join("task.toml"),
            "id = \"t1\"\nprompt = \"p\"\ntimeout_s = 10\ntags = []\n[scorer]\nchecks = []\n",
        )
        .unwrap();
        fs::write(
            task_dir.join("trace.jsonl"),
            "{\"timestamp\":\"t\",\"sessionId\":\"s\",\"type\":\"assistant_message\",\"content\":\"done\",\"toolCalls\":[]}\n",
        )
        .unwrap();
    }

    /// Write a task dir whose fixture is missing → `run_task` yields an
    /// `Error` outcome (harness-internal failure).
    fn write_error_task(task_dir: &Path) {
        fs::create_dir_all(task_dir).unwrap();
        fs::write(
            task_dir.join("task.toml"),
            "id = \"t2\"\nprompt = \"p\"\ntimeout_s = 10\ntags = []\n[scorer]\nchecks = []\n",
        )
        .unwrap();
        fs::write(
            task_dir.join("trace.jsonl"),
            "{\"timestamp\":\"t\",\"sessionId\":\"s\",\"type\":\"assistant_message\",\"content\":\"done\",\"toolCalls\":[]}\n",
        )
        .unwrap();
        // No `fixture/` dir → make_temp_workspace fails → Outcome::Error.
    }

    /// Scoring a single passing task yields one `Pass` entry with fitness 1.0.
    #[test]
    fn score_pass_entry() {
        let dir = TempDir::new("pass");
        let evals = dir.path().join("evals");
        fs::create_dir_all(&evals).unwrap();
        write_pass_task(&evals.join("t1"));

        let report = score_artifact(&evals, Variant::Golden).unwrap();
        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.task_id, "t1");
        assert_eq!(e.fitness, Some(1.0));
        assert!(matches!(e.outcome, Outcome::Pass));
        assert_eq!(report.mean_fitness(), Some(1.0));
    }

    /// Committing a scored report appends a node with the mean fitness, and
    /// `best_parent` / `recommend` see it.
    #[test]
    fn commit_node_with_mean_fitness() {
        let dir = TempDir::new("commit");
        let evals = dir.path().join("evals");
        fs::create_dir_all(&evals).unwrap();
        write_pass_task(&evals.join("t1"));

        let report = score_artifact(&evals, Variant::Golden).unwrap();
        let mut lin = Lineage::default();
        let id = commit_artifact(&mut lin, "artifact://t1".into(), &report, 1_000).unwrap();

        assert_eq!(lin.nodes().len(), 1);
        let node = &lin.nodes()[0];
        assert_eq!(node.id, id);
        assert_eq!(node.fitness, Some(1.0));
        assert!(node.parents.is_empty(), "first node has no parent");
        assert_eq!(node.artifact_ref, "artifact://t1");

        // best_parent and recommend both see the committed node.
        assert_eq!(lin.best_parent().unwrap().id, id);
        assert_eq!(recommend(&lin).unwrap().id, id);
    }

    /// A second commit chains to the current head as its parent.
    #[test]
    fn commit_chains_to_head() {
        let dir = TempDir::new("chain");
        let evals = dir.path().join("evals");
        fs::create_dir_all(&evals).unwrap();
        write_pass_task(&evals.join("t1"));

        let report = score_artifact(&evals, Variant::Golden).unwrap();
        let mut lin = Lineage::default();
        let first = commit_artifact(&mut lin, "a".into(), &report, 1_000).unwrap();
        let second = commit_artifact(&mut lin, "b".into(), &report, 2_000).unwrap();

        let n2 = &lin.nodes()[1];
        assert_eq!(n2.parents, vec![first]);
        assert_eq!(lin.head().unwrap().id, second);
    }

    /// An empty evals dir yields the typed `NoTasks` error.
    #[test]
    fn empty_evals_dir_typed_error() {
        let dir = TempDir::new("empty");
        let evals = dir.path().join("evals");
        fs::create_dir_all(&evals).unwrap();

        let err = score_artifact(&evals, Variant::Golden).unwrap_err();
        assert!(
            matches!(err, OptimizerError::NoTasks(_)),
            "expected NoTasks, got {err:?}"
        );
    }

    /// A missing evals dir yields the typed `EvalsDirMissing` error.
    #[test]
    fn missing_evals_dir_typed_error() {
        let dir = TempDir::new("missing");
        let evals = dir.path().join("does-not-exist");

        let err = score_artifact(&evals, Variant::Golden).unwrap_err();
        assert!(
            matches!(err, OptimizerError::EvalsDirMissing(_)),
            "expected EvalsDirMissing, got {err:?}"
        );
    }

    /// An all-Error report commits a node with `None` fitness (no crash), and
    /// that node is ineligible for `best_parent`.
    #[test]
    fn all_error_none_fitness() {
        let dir = TempDir::new("allerr");
        let evals = dir.path().join("evals");
        fs::create_dir_all(&evals).unwrap();
        write_error_task(&evals.join("t2"));

        let report = score_artifact(&evals, Variant::Golden).unwrap();
        assert_eq!(report.entries.len(), 1);
        let e = &report.entries[0];
        assert_eq!(e.fitness, None);
        assert!(matches!(e.outcome, Outcome::Error(_)));
        assert_eq!(report.mean_fitness(), None);

        let mut lin = Lineage::default();
        let id = commit_artifact(&mut lin, "artifact://t2".into(), &report, 1_000).unwrap();
        assert_eq!(lin.nodes().len(), 1);
        assert_eq!(lin.nodes()[0].id, id);
        assert_eq!(lin.nodes()[0].fitness, None);
        // Unscored → ineligible for best_parent / recommend.
        assert!(lin.best_parent().is_none());
        assert!(recommend(&lin).is_none());
    }

    /// A mixed Pass + Fail report averages to 0.5 (Error excluded).
    #[test]
    fn mixed_pass_fail_mean() {
        let dir = TempDir::new("mixed");
        let evals = dir.path().join("evals");
        fs::create_dir_all(&evals).unwrap();
        // t1 passes (no checks).
        write_pass_task(&evals.join("t1"));
        // t2 fails: a file check that cannot be satisfied.
        let t2 = evals.join("t2");
        fs::create_dir_all(t2.join("fixture")).unwrap();
        fs::write(
            t2.join("task.toml"),
            "id = \"t2\"\nprompt = \"p\"\ntimeout_s = 10\ntags = []\n\
             [[scorer.checks]]\nid = \"c1\"\nkind = \"file\"\npath = \"nope.txt\"\nexpect_contains = \"x\"\n",
        )
        .unwrap();
        fs::write(
            t2.join("trace.jsonl"),
            "{\"timestamp\":\"t\",\"sessionId\":\"s\",\"type\":\"assistant_message\",\"content\":\"done\",\"toolCalls\":[]}\n",
        )
        .unwrap();

        let report = score_artifact(&evals, Variant::Golden).unwrap();
        assert_eq!(report.entries.len(), 2);
        // t1 first (sorted), t2 second.
        assert_eq!(report.entries[0].fitness, Some(1.0));
        assert_eq!(report.entries[1].fitness, Some(0.0));
        assert_eq!(report.mean_fitness(), Some(0.5));
    }

    /// The ISO-8601 conversion matches known values and is lexicographically
    /// ordered by time.
    #[test]
    fn iso8601_known_values() {
        // 1970-01-01T00:00:00.000Z
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00.000Z");
        // 2026-01-01T00:00:00.000Z
        assert_eq!(iso8601_utc(1_767_225_600_000), "2026-01-01T00:00:00.000Z");
        // Lexicographic ordering matches chronological ordering.
        assert!(iso8601_utc(1_000) < iso8601_utc(2_000));
    }
}
