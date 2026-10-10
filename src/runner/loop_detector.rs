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
        let threshold = threshold.max(1);
        let window = window.max(threshold * 3);
        Self {
            window,
            threshold,
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
        if self.detect_cycle() {
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

    /// Checks if the history ends with a periodic pattern of period p in 1..=3
    /// repeating at least `threshold` times.
    fn detect_cycle(&self) -> bool {
        let n = self.history.len();
        for p in 1..=3 {
            let required = p * self.threshold;
            if n < required {
                continue;
            }
            let mut matches = true;
            for k in 1..self.threshold {
                for i in 0..p {
                    let curr = self.history[n - 1 - i];
                    let prev = self.history[n - 1 - i - k * p];
                    if curr != prev {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    break;
                }
            }
            if matches {
                return true;
            }
        }
        false
    }
}

fn hash_action(name: &str, args: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    args.hash(&mut hasher);
    hasher.finish()
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

    #[test]
    fn alternating_actions_cycle_detected() {
        let mut d = LoopDetector::new(6, 3);
        // Period 2: A, B repeating 3 times (6 actions total)
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        // 3rd period completes: advisory fires
        assert_eq!(d.record("read", "foo"), LoopState::Advisory);
        // Next repeat: loop detected
        assert_eq!(d.record("bash", "ls"), LoopState::LoopDetected);
    }

    #[test]
    fn three_step_cycle_detected() {
        let mut d = LoopDetector::new(6, 3);
        // Period 3: A, B, C repeating 3 times (9 actions total)
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
        assert_eq!(d.record("edit", "bar"), LoopState::Ok);

        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
        assert_eq!(d.record("edit", "bar"), LoopState::Ok);

        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
        // 3rd period completes: advisory fires
        assert_eq!(d.record("edit", "bar"), LoopState::Advisory);
        // Next action in cycle: loop detected
        assert_eq!(d.record("bash", "ls"), LoopState::LoopDetected);
    }

    #[test]
    fn cycle_interrupted_does_not_loop() {
        let mut d = LoopDetector::new(6, 3);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
        // Interrupted by a different command
        assert_eq!(d.record("bash", "pwd"), LoopState::Ok);
        assert_eq!(d.record("bash", "ls"), LoopState::Ok);
        assert_eq!(d.record("read", "foo"), LoopState::Ok);
    }
}
