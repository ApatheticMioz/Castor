//! Turn budget and loop detector constants for the runner.

/// Default per-dispatch turn budget when `options.turns_budget` is unset.
pub const DEFAULT_TURNS_BUDGET: u32 = 80;

/// Number of non-mutating bash probes allowed before an advisory is injected.
pub const DEFAULT_PROBE_BUDGET: usize = 4;

/// Remaining turns threshold below which a landing notice is injected.
pub(crate) const LANDING_WINDOW: u32 = 5;

/// Number of recent tool calls tracked by the sliding-window loop detector.
pub(crate) const LOOP_WINDOW: usize = 6;

/// Threshold of identical/alternating cycles to trigger loop detection.
pub(crate) const LOOP_THRESHOLD: usize = 3;

