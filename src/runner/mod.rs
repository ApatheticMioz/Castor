//! Multi-turn autonomous agent execution loop and session orchestration.
//!
//! # Architecture
//! The runner subsystem is decomposed into focused submodules:
//! - [`session`] — unconstrained multi-turn session loop and terminal outcome resolution.
//! - [`messages`] — message shaping, reasoning tag stripping, tool delimiter sanitization.
//! - [`context`] — token budgeting, in-flight ceilings, and session history persistence.
//! - [`salvage`] — reasoning budget ceiling detection and markdown salvage reporting.
//! - [`watchdog`] — wall-clock timeout watchdog and token monitoring.
//! - [`budget`] — turn budget, probe budget, and landing window constants.
//! - [`skeleton`] — tool output spilling and deterministic workspace skeleton generation.
//! - [`loop_detector`] — action-hash sliding window loop breaker.
//! - [`events`] — session JSONL event logging.

pub mod budget;
pub mod context;
pub mod events;
pub mod loop_detector;
pub mod messages;
pub mod salvage;
pub mod session;
pub mod skeleton;
pub mod watchdog;

#[cfg(test)]
mod tests;

pub use session::run_session;

use std::path::PathBuf;

use async_trait::async_trait;
use thiserror::Error;

use crate::engine::{EngineClient, EngineError, Message, ToolSchema};
use crate::state::StateDir;

use budget::DEFAULT_PROBE_BUDGET;

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a tool by name with JSON arguments.
    async fn execute(&self, name: &str, args_json: &str) -> Result<ToolOutcome, ToolError>;
}

/// The result of a tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub text: String,
}

/// An error from a tool execution.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool '{name}' failed: {message}")]
    Execute { name: String, message: String },
}

/// The async trait the runner depends on for engine chat.
///
/// [`EngineClient`] implements this; tests inject a scripted engine.
#[async_trait]
pub trait ChatEngine: Send + Sync {
    /// One chat turn: send messages + tool schemas, get a completion,
    /// optionally carrying a per-session reasoning_effort tier.
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        stream: bool,
        reasoning_effort: Option<&str>,
    ) -> Result<crate::engine::Completion, EngineError>;

    /// Exact prompt token count via the engine's tokenizer endpoint when
    /// available; `None` falls back to `usage.prompt_tokens` / local sizing.
    async fn count_tokens(
        &self,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Option<usize> {
        None
    }
}

#[async_trait]
impl ChatEngine for EngineClient {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        stream: bool,
        reasoning_effort: Option<&str>,
    ) -> Result<crate::engine::Completion, EngineError> {
        EngineClient::chat(self, messages, tools, stream, reasoning_effort).await
    }

    async fn count_tokens(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Option<usize> {
        EngineClient::count_tokens(self, messages, tools).await
    }
}

/// The result of a session run.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub final_text: String,
    pub turns: u32,
    /// Terminal status: `"completed"`, `"reasoning_budget_exhausted"`, or `"failed"`.
    ///
    /// `"reasoning_budget_exhausted"` is the honest terminal status for a
    /// session that stopped at its reasoning ceiling. It is *never* masked as a
    /// success; the salvage pass that recovers a plain-text report on that path
    /// is annotated in [`Self::final_text`].
    pub status: String,
}

/// An error from the session runner.
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("engine error: {0}")]
    Engine(#[from] EngineError),
    #[error("loop detected: repeated action '{name}'")]
    LoopDetected { name: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct LeaseYieldContext {
    pub sem: std::sync::Arc<crate::task::semaphore::TaskSemaphore>,
    pub lease: std::sync::Arc<std::sync::Mutex<crate::task::semaphore::SlotLease>>,
    pub task_id: String,
}

impl std::fmt::Debug for LeaseYieldContext {
    /// `sem` and `lease` hold non-`Debug` handles (`oneshot::Sender` inside the
    /// wait-queue; the slot lease file path), so we print only the task id —
    /// enough to log a yield decision without pulling in the whole semaphore
    /// state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseYieldContext")
            .field("task_id", &self.task_id)
            .finish()
    }
}

/// State / workspace context the runner uses for terminal artifacts.
///
/// This is deliberately *only* the durable-location context — not the engine,
/// executor, or budget — because those already flow through [`run_session`]
/// directly. It exists so a session can persist a salvage report outside the
/// event ledger at terminal.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// The castor state directory. `.scratch/` salvage reports are written here
    /// (falling back to the workspace when unset).
    pub state: Option<StateDir>,
    /// The session's working directory. Used as the fallback `.scratch/`
    /// location when no state dir is available.
    pub workspace: Option<PathBuf>,
    /// Per-session reasoning-effort tier, forwarded to the engine on every
    /// chat turn. `None` means "no override": the client sends
    /// neither `reasoning_effort` nor `chat_template_kwargs` and the
    /// server-side default applies (zero behaviour change).
    pub reasoning_effort: Option<String>,
    /// Consecutive non-mutating bash probes before the probe-budget advisory
    /// is injected. Defaults to 4.
    pub probe_budget: usize,
    /// The engine's max context (in tokens). Drives two in-session gates:
    ///
    /// - **75% resume watermark** — a resumed session rolls to a fresh
    ///   `[system, user]` pair when the loaded history plus the new prompt
    ///   would exceed 75% of this budget (checked once, at resume, in
    ///   [`should_resume`], tokens approximated as `chars / 4`).
    /// - **90% in-flight ceiling** — while a session runs, [`run_session`]
    ///   checks the accumulated context (preferring engine-reported
    ///   `prompt_tokens` plus uncounted tail tokens) against 90% of this
    ///   budget at the top of every turn; when reached, it disables further
    ///   tool dispatch, appends [`context::CONTEXT_CEILING_DIRECTIVE`], and
    ///   lands the final completion in the remaining 10% headroom.
    ///
    /// `0` (the default) disables both gates — history always resumes when
    /// present and the in-flight ceiling never trips (zero behavior change for
    /// callers that don't set it).
    pub max_context: usize,
    /// Reasoning open tag the runner strips from assistant content and
    /// sanitizes inside tool results. `None` means "use the reserved default"
    /// (`concat!("<th","ink>")`), so the raw byte sequence never appears in
    /// source and the tokenizer never sees a contiguous reasoning delimiter.
    pub reasoning_open_tag: Option<String>,
    /// Reasoning close tag (the paired `</...>` form). `None` uses the default
    /// (`concat!("</th","ink>")`).
    pub reasoning_close_tag: Option<String>,
    /// When `Some`, the runner performs per-turn slot-bookkeeping and
    /// cooperative yielding against the shared task-slot pool (Issues #23 /
    /// #28): after each turn it records the turn's token footprint on the
    /// held lease and, if the pool's yield gate fires, it releases and
    /// re-acquires the lease to hand the slot to a younger / more
    /// cache-efficient worker.
    pub lease_context: Option<LeaseYieldContext>,
    /// Per-turn watchdog wall-clock ceiling (seconds). When `Some(s)` with
    /// `s > 0`, each `engine.chat` call is wrapped in
    /// [`tokio::time::timeout`]. A timeout triggers a single retry at `"low"`
    /// reasoning effort. `None` disables the timeout.
    pub turn_timeout_secs: Option<u64>,
    /// Per-turn watchdog reasoning-token budget. When `Some(n)` with
    /// `n > 0`, a turn that returns `finish_reason == "length"` with empty
    /// `tool_calls` and `metrics.reasoning_tokens >= n` is treated as a
    /// stall and retried once at `"low"` reasoning effort.
    /// `None` or `0` disables the token-budget trigger.
    pub max_turn_reasoning_tokens: Option<u32>,
    /// Maximum bytes of a single tool output before it is spilled
    /// to disk (keeping only a head/tail preview in the conversation).
    /// `None` means "use the default" ([`SPILL_THRESHOLD_BYTES`] = 16384).
    pub tool_spill_bytes: Option<usize>,
}

impl SessionOptions {
    /// Build options from just a state directory (the common worker case).
    pub fn with_state(state: StateDir) -> Self {
        Self {
            state: Some(state),
            workspace: None,
            reasoning_effort: None,
            probe_budget: DEFAULT_PROBE_BUDGET,
            max_context: 0,
            reasoning_open_tag: None,
            reasoning_close_tag: None,
            lease_context: None,
            turn_timeout_secs: None,
            max_turn_reasoning_tokens: None,
            tool_spill_bytes: None,
        }
    }

    /// Build options from just a workspace path (e.g. an offline replay).
    pub fn with_workspace(workspace: PathBuf) -> Self {
        Self {
            state: None,
            workspace: Some(workspace),
            reasoning_effort: None,
            probe_budget: DEFAULT_PROBE_BUDGET,
            max_context: 0,
            reasoning_open_tag: None,
            reasoning_close_tag: None,
            lease_context: None,
            turn_timeout_secs: None,
            max_turn_reasoning_tokens: None,
            tool_spill_bytes: None,
        }
    }

    /// Attach the per-turn stall watchdog. Wraps each
    /// `engine.chat` call in a wall-clock timeout (`turn_timeout_secs`, `0`
    /// disables) and treats a `length`/empty-tool-call turn that burned at
    /// least `max_turn_reasoning_tokens` reasoning tokens (`0` disables) as a
    /// stall; either trigger retries the turn once at `"low"` reasoning
    /// effort. Returns `Self` for chaining.
    pub fn with_turn_watchdog(
        self,
        turn_timeout_secs: u64,
        max_turn_reasoning_tokens: u32,
    ) -> Self {
        Self {
            turn_timeout_secs: Some(turn_timeout_secs),
            max_turn_reasoning_tokens: Some(max_turn_reasoning_tokens),
            ..self
        }
    }

    /// Attach a [`LeaseYieldContext`] so the runner performs per-turn slot
    /// bookkeeping and cooperative yielding (Issues #23 / #28). Returns `Self`
    /// for chaining.
    pub fn with_lease_context(mut self, ctx: LeaseYieldContext) -> Self {
        self.lease_context = Some(ctx);
        self
    }

    /// Set a provider-specific reasoning tag pair (e.g. `<reasoning>` /
    /// `</reasoning>` for providers that do not use the reserved delimiters).
    /// Both tags must be set together; a bare open tag is not a valid pair.
    pub fn with_reasoning_tags(
        mut self,
        open: impl Into<String>,
        close: impl Into<String>,
    ) -> Self {
        self.reasoning_open_tag = Some(open.into());
        self.reasoning_close_tag = Some(close.into());
        self
    }

    /// Resolve the effective open tag: the configured value when present,
    /// otherwise the reserved default built via `concat!`.
    pub fn effective_open_tag(&self) -> String {
        self.reasoning_open_tag
            .clone()
            .unwrap_or_else(|| concat!("<th", "ink>").to_string())
    }

    /// Resolve the effective close tag: the configured value when present,
    /// otherwise the reserved default built via `concat!`.
    pub fn effective_close_tag(&self) -> String {
        self.reasoning_close_tag
            .clone()
            .unwrap_or_else(|| concat!("</th", "ink>").to_string())
    }

    /// The directory `.scratch/` salvage reports are written under: the state
    /// dir when available, otherwise the workspace. `None` when neither is
    /// set (the salvage is then skipped with a clean log, not a crash).
    pub fn scratch_root(&self) -> Option<PathBuf> {
        self.state
            .as_ref()
            .map(|s| s.root().to_path_buf())
            .or_else(|| self.workspace.clone())
    }
}

#[cfg(test)]
pub(crate) fn should_resume(
    history: &[Message],
    new_user_prompt: &str,
    options: Option<&SessionOptions>,
) -> bool {
    let max_context = options.map(|o| o.max_context).unwrap_or(0);
    context::should_resume(history, new_user_prompt, max_context)
}
