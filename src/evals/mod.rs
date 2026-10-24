//! Evals data model: strict typed parsing of the eval fixture formats
//! defined in `evals/EVALS.md` (`task.toml` + `trace.jsonl`).
//!
//! `fixture` is the data model; `replay` is the trace-replaying engine;
//! `runner` glues them together with the real session loop and the
//! deterministic scorer into a single `run_task` entry point.

pub mod fixture;
pub mod replay;
pub mod runner;

pub use fixture::{
    Check, Scorer, Task, TaskConfig, TaskError, ToolCallFunction, ToolCallRef, Trace, TraceError,
    TraceStep,
};
pub use replay::ReplayEngine;
pub use runner::{EvalReport, Outcome, Variant};
