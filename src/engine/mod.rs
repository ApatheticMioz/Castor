//! Engine orchestration: OpenAI-compatible chat client and lifecycle.

pub mod backend;
mod detached;
pub mod lifecycle;
pub mod provider;
pub(crate) mod supervisor;

pub use backend::{
    Backend, EffortSerializer, EngineCapabilities, ProbeShape, ReadinessKind, ReasoningWireFormat,
    StreamSentinel,
};
pub use lifecycle::{EngineLifecycle, LifecycleError};
pub use provider::{Completion, EngineClient, EngineError, Message, Metrics, ToolCall, ToolSchema};
