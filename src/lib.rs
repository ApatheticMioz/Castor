//! Castor: local-first agent microkernel.
//!
//! Provides the core libraries for MCP serving, zero-trust sandbox execution,
//! structural AST surgery, stream proxying, task lifecycle, and offline evolution.

pub mod cli;
pub mod config;
pub mod engine;
pub mod evals;
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

pub use cli::{evo_status, run_clean, run_evo, run_install, run_server};
