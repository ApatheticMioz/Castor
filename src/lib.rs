//! Castor: local-first agent microkernel.
//!
//! Provides the core libraries for MCP serving, zero-trust sandbox execution,
//! structural AST surgery, stream proxying, task lifecycle, and offline evolution.

pub mod cli;
pub mod config;
pub mod engine;
#[cfg(feature = "evals")]
pub mod evals;
#[cfg(feature = "evo")]
pub mod evo;
pub mod mcp;
pub mod platform;
pub mod proxy;
pub mod pruner;
pub mod runner;
pub mod skills;
pub mod state;
pub mod task;
pub mod telemetry;
pub mod tools;

#[cfg(feature = "evo")]
pub use cli::{evo_status, run_evo};
pub use cli::{run_clean, run_install, run_server};
