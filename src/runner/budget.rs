//! Loop detector and probe budget constants for the runner.

/// Number of non-mutating bash probes allowed before an advisory is injected.
pub const DEFAULT_PROBE_BUDGET: usize = 4;

/// Number of recent tool calls tracked by the sliding-window loop detector.
pub(crate) const LOOP_WINDOW: usize = 6;

/// Threshold of identical/alternating cycles to trigger loop detection.
pub(crate) const LOOP_THRESHOLD: usize = 3;
