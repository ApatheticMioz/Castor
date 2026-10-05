//! Sliding-window action-hash loop detector.
//!
//! Hashes each executed action (tool name + args) and keeps a window of the
//! most recent hashes. When the same action repeats `threshold` consecutive
//! times, an advisory is injected once; if it persists, the session is flagged
//! as a loop and must stop.

use std::collections::{VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};

/// Outcome of recording an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopState {
    /// No loop detected.
    Ok,
    /// A loop was detected; an advisory has been injected (once).
    Advisory,
    /// The loop persisted after the advisory; the session must stop.
    LoopDetected,
}

/// Sliding-window detector for repeated identical actions.
#[derive(Debug)]
pub struct LoopDetector {
    window: usize,
    threshold: usize,
    history: VecDeque<u64>,
    advisory_injected: bool,
}

impl LoopDetector {
    pub fn new(window: usize, threshold: usize) -> Self {
        let window = window.max(1);
        Self {
            window,
            threshold: threshold.max(1),
            history: VecDeque::with_capacity(window),
            advisory_injected: false,
        }
    }

    /// Record an executed action and report the loop state.
    pub fn record(&mut self, name: &str, args: &str) -> LoopState {
        let hash = hash_action(name, args);
        self.history.push_back(hash);
        if self.history.len() > self.window {
            self.history.pop_front();
        }
        let last = *self.history.back().expect("history is non-empty");
        let mut count = 0;
        for h in self.history.iter().rev() {
            if *h == last {
                count += 1;
            } else {
                break;
            }
        }
        if count >= self.threshold {
            if self.advisory_injected {
                LoopState::LoopDetected
            } else {
                self.advisory_injected = true;
                LoopState::Advisory
            }
        } else {
            LoopState::Ok
        }
    }
}

fn hash_action(name: &str, args: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    args.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// ProbeTracker: consecutive non-mutating probe budget
// ---------------------------------------------------------------------------

/// Tool names that modify files (reset the probe counter).
const MUTATING_TOOLS: &[&str] = &["write_file", "edit_file", "ast_replace", "apply_patch"];

fn is_mutating_tool(name: &str) -> bool {
    MUTATING_TOOLS.contains(&name)
}

/// The probe-budget advisory injected when the model has executed `budget`
/// consecutive non-mutating, non-scratchpad bash commands without making
/// any file changes.
pub const PROBE_ADVISORY: &str = "[Probe Advisory] You have executed consecutive non-mutating \
    exploratory probes without modifying files. Proceed with targeted AST edits or code mutations.";

/// Tracks consecutive non-mutating exploratory probes (`bash` commands that
/// do not target `.scratch/`) and signals when the probe budget is exhausted.
///
/// Mutating tools (`write_file`, `edit_file`, `ast_replace`, `apply_patch`)
/// reset the counter.  Commands that reference a `.scratch/` path component
/// are exempt — they are empirical diagnostics, not idle ping-pong.
/// Outcome of recording an exploratory probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeState {
    /// Probe count is within normal bounds.
    Ok,
    /// Probe budget reached: inject advisory to guide model toward mutations.
    Advisory,
    /// Probe streak exceeded impasse ceiling: halt exploratory loop and yield findings.
    Impasse,
}

/// Tracks consecutive non-mutating exploratory probes (`bash` commands that
/// do not target `.scratch/`) and signals when the probe budget is reached
/// or when an impasse is encountered.
///
/// Mutating tools (`write_file`, `edit_file`, `ast_replace`, `apply_patch`)
/// reset the counter. Commands that reference a `.scratch/` path component
/// are exempt — they are empirical diagnostics, not idle ping-pong.
#[derive(Debug)]
pub struct ProbeTracker {
    budget: usize,
    consecutive_probes: usize,
    advisory_injected: bool,
}

impl ProbeTracker {
    pub fn new(budget: usize) -> Self {
        Self {
            budget: budget.max(1),
            consecutive_probes: 0,
            advisory_injected: false,
        }
    }

    /// Record a tool execution and evaluate probe state.
    pub fn record(&mut self, name: &str, args: &str) -> ProbeState {
        // Mutating tools reset the probe streak.
        if is_mutating_tool(name) {
            self.consecutive_probes = 0;
            return ProbeState::Ok;
        }
        // Only `bash` commands are tracked as probes.
        if name != "bash" {
            return ProbeState::Ok;
        }
        // Scratchpad commands are exempt (empirical diagnostics).
        if targets_scratchpad(args) {
            return ProbeState::Ok;
        }
        // Non-scratchpad bash command: count it.
        self.consecutive_probes += 1;
        if self.consecutive_probes >= self.budget * 3 {
            ProbeState::Impasse
        } else if self.consecutive_probes >= self.budget && !self.advisory_injected {
            self.advisory_injected = true;
            ProbeState::Advisory
        } else {
            ProbeState::Ok
        }
    }

    /// Number of consecutive exploratory probes currently recorded.
    pub fn consecutive_probes(&self) -> usize {
        self.consecutive_probes
    }
}

/// Returns `true` when the command references a `.scratch/` path component.
///
/// Tokenises the command on whitespace and checks each token for a
/// path component equal to `.scratch` (handles `.scratch/foo.py`,
/// `/.abs/path/.scratch/x`, `cd .scratch && …`, etc.).
fn targets_scratchpad(cmd: &str) -> bool {
    for token in cmd.split_whitespace() {
        let t = token.trim_matches(|c: char| c == '"' || c == '\'' || c == '/' || c == '\\');
        for part in t.split(['/', '\\']) {
            if part == ".scratch" {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_actions_never_loop() {
        let mut d = LoopDetector::new(6, 3);
        for i in 0..10 {
            assert_eq!(d.record("read", &format!("file_{i}")), LoopState::Ok);
        }
    }

    #[test]
    fn identical_actions_advisory_then_detected() {
        let mut d = LoopDetector::new(6, 3);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Advisory);
        assert_eq!(d.record("bash", "ls"), LoopState::LoopDetected);
    }

    #[test]
    fn a_different_action_resets_the_streak() {
        let mut d = LoopDetector::new(6, 3);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok); // streak=1
        assert_eq!(d.record("bash", "ls"), LoopState::Ok); // streak=2
        assert_eq!(d.record("read", "other"), LoopState::Ok); // reset, streak=1
        assert_eq!(d.record("bash", "ls"), LoopState::Ok); // streak=1
        assert_eq!(d.record("bash", "ls"), LoopState::Ok); // streak=2
        // First threshold hit after the reset → advisory (not a hard stop).
        assert_eq!(d.record("bash", "ls"), LoopState::Advisory);
    }

    #[test]
    fn advisory_latches_then_hard_stops() {
        let mut d = LoopDetector::new(6, 3);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Advisory);
        // The advisory is injected once; the next repetition is a hard stop.
        assert_eq!(d.record("bash", "ls"), LoopState::LoopDetected);
    }

    // --- ProbeTracker --------------------------------------------------------

    #[test]
    fn probe_budget_trips_at_threshold() {
        let mut t = ProbeTracker::new(3);
        // First two probes: below the budget.
        assert_eq!(t.record("bash", "{\"command\":\"ls\"}"), ProbeState::Ok);
        assert_eq!(t.record("bash", "{\"command\":\"pwd\"}"), ProbeState::Ok);
        // Third probe: at the budget → advisory fires.
        assert_eq!(
            t.record("bash", "{\"command\":\"cat file.txt\"}"),
            ProbeState::Advisory
        );
        // The advisory latches; subsequent probes return Ok until impasse.
        assert_eq!(
            t.record("bash", "{\"command\":\"head -1 file.txt\"}"),
            ProbeState::Ok
        );
    }

    #[test]
    fn probe_budget_trips_impasse_at_ceiling() {
        let mut t = ProbeTracker::new(2);
        assert_eq!(t.record("bash", "{\"command\":\"ls\"}"), ProbeState::Ok);
        assert_eq!(
            t.record("bash", "{\"command\":\"pwd\"}"),
            ProbeState::Advisory
        );
        assert_eq!(t.record("bash", "{\"command\":\"p3\"}"), ProbeState::Ok);
        assert_eq!(t.record("bash", "{\"command\":\"p4\"}"), ProbeState::Ok);
        assert_eq!(t.record("bash", "{\"command\":\"p5\"}"), ProbeState::Ok);
        // At budget * 3 (6 probes):
        assert_eq!(
            t.record("bash", "{\"command\":\"p6\"}"),
            ProbeState::Impasse
        );
    }

    #[test]
    fn mutating_tools_reset_probe_count() {
        let mut t = ProbeTracker::new(2);
        assert_eq!(t.record("bash", "{\"command\":\"ls\"}"), ProbeState::Ok);
        // A write_file resets the streak.
        assert_eq!(
            t.record("write_file", "{\"path\":\"a.rs\"}"),
            ProbeState::Ok
        );
        // After the reset, one more probe is still below budget.
        assert_eq!(t.record("bash", "{\"command\":\"pwd\"}"), ProbeState::Ok);
        // Second probe after reset → advisory.
        assert_eq!(
            t.record("bash", "{\"command\":\"cat b\"}"),
            ProbeState::Advisory
        );
    }

    #[test]
    fn scratchpad_commands_are_exempt() {
        let mut t = ProbeTracker::new(1);
        // Budget of 1 would trip on the very first non-scratch probe,
        // but scratchpad commands never count.
        assert_eq!(
            t.record("bash", "{\"command\":\"python .scratch/repro.py\"}"),
            ProbeState::Ok
        );
        assert_eq!(
            t.record("bash", "{\"command\":\"bash .scratch/run.sh\"}"),
            ProbeState::Ok
        );
        assert_eq!(
            t.record("bash", "{\"command\":\"cat /abs/path/.scratch/dump.txt\"}"),
            ProbeState::Ok
        );
        assert_eq!(
            t.record("bash", "{\"command\":\"ls .scratch/\"}"),
            ProbeState::Ok
        );
    }

    #[test]
    fn non_scratch_bash_is_counted() {
        let mut t = ProbeTracker::new(1);
        // A regular bash command (no .scratch/) trips immediately.
        assert_eq!(
            t.record("bash", "{\"command\":\"ls -la\"}"),
            ProbeState::Advisory
        );
    }

    #[test]
    fn non_bash_tools_do_not_count_as_probes() {
        let mut t = ProbeTracker::new(1);
        // read_file, search_code, etc. are non-mutating but not probes.
        assert_eq!(t.record("read_file", "{\"path\":\"a.rs\"}"), ProbeState::Ok);
        assert_eq!(
            t.record("search_code", "{\"query\":\"foo\"}"),
            ProbeState::Ok
        );
    }

    #[test]
    fn scratchpad_path_detection() {
        assert!(targets_scratchpad("python .scratch/repro.py"));
        assert!(targets_scratchpad("bash .scratch/run.sh"));
        assert!(targets_scratchpad("cat /abs/path/.scratch/dump.txt"));
        assert!(targets_scratchpad("ls .scratch/"));
        assert!(targets_scratchpad("cd .scratch && ls"));
        assert!(!targets_scratchpad("ls -la"));
        assert!(!targets_scratchpad("cargo test"));
        // `.scratch` as part of a longer name should NOT match.
        assert!(!targets_scratchpad("cat .scratchpad/file.txt"));
    }
}
