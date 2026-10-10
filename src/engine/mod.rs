//! Engine orchestration: OpenAI-compatible chat client and lifecycle.

pub(crate) mod backend;
mod detached;
pub mod lifecycle;
pub mod provider;
pub(crate) mod supervisor;

pub use lifecycle::{EngineLifecycle, LifecycleError};
pub use provider::{Completion, EngineClient, EngineError, Message, Metrics, ToolCall, ToolSchema};
