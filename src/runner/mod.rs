//! Session runner: the multi-turn agent loop.
//!
//! - [`ToolExecutor`]: the async trait the runner depends on for tool execution.
//! - [`ChatEngine`]: the async trait the runner depends on for engine chat.
//! - [`run_session`]: build messages (tool schemas last, vLLM APC contract) →
//!   engine chat → execute tool calls → append results → continue until the
//!   model produces a final or the turn budget is exhausted.
//!
//! Named sessions persist their sanitized conversation history to
//! `<state>/sessions/<id>/messages.json` (end of each turn + normal
//! completion) and resume it at the start of a later dispatch, subject to
//! the 75% `max_context` watermark.
//!
//! Budgets, cooperative landing, loop detection, and the JSONL event ledger
//! live here; the loop detector and event logger are split into submodules.

pub mod events;
pub mod loop_detector;

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::engine::{EngineClient, EngineError, Message, Metrics, ToolCall, ToolSchema};
use crate::state::StateDir;

use events::EventLogger;
use loop_detector::{LoopDetector, LoopState};

/// Default turn budget (from the task record; extendable up to `MAX_ELASTIC_TURNS`).
pub const DEFAULT_TURNS_BUDGET: u32 = 80;

/// Turns before the end of the budget at which a landing notice is injected.
const LANDING_WINDOW: u32 = 5;

/// `finish_reason` value the engine reports when a response is truncated at
/// its reasoning/output token ceiling. This is the signal for a
/// `reasoning_budget_exhausted` termination.
const REASONING_CEILING_FINISH: &str = "length";

/// Peer-empowered salvage prompt injected when the reasoning ceiling is hit.
///
/// Asks the autonomous peer engineer to synthesize what it has verified,
/// implementation state, data tables, and exact blockers, with full tools
/// available to ground its findings.
const SALVAGE_PROMPT: &str = "[Salvage] Reasoning budget ceiling reached for this dispatch. \
    As an autonomous peer engineer, provide your collaborative status report: \
    (1) summarize the findings, conclusions, and implementations you have reached so far, \
    (2) present any data tables, metrics, or scratchpad artifacts produced, and \
    (3) identify remaining blockers, failing test coordinates, or open questions for the next slice. \
    You have full tools available if you need to inspect scratchpad outputs or verify test logs to ground your report.";

/// A terminal turn where the engine stopped at its reasoning/output ceiling:
/// no further tool calls were issued and the finish reason is the ceiling
/// sentinel (`"length"`).
fn is_reasoning_ceiling(completion: &crate::engine::Completion) -> bool {
    completion.tool_calls.is_empty()
        && completion
            .finish_reason
            .as_deref()
            .is_some_and(|f| f == REASONING_CEILING_FINISH)
}

/// Sliding-window size for loop detection (action hashes).
const LOOP_WINDOW: usize = 6;

/// Consecutive identical actions that trip loop detection.
const LOOP_THRESHOLD: usize = 3;

/// Default probe budget: consecutive non-mutating bash probes before the
/// probe-budget advisory is injected.
pub const DEFAULT_PROBE_BUDGET: usize = 4;

/// The async trait the runner depends on for tool execution.
///
/// The concrete tools implement this trait; the runner depends only on it.
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
}

/// The result of a session run.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub final_text: String,
    pub turns: u32,
    /// The effective turn budget that was applied.
    pub budget: u32,
    /// Terminal status: `"completed"`, `"completed_budget_exhausted"`,
    /// `"reasoning_budget_exhausted"`, or `"failed"`.
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

fn msg(role: &str, content: impl Into<String>) -> Message {
    Message {
        role: role.into(),
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

/// Strip <th_ink>...</th_ink> reasoning traces from an assistant message before
/// persisting it in multi-turn conversation history, preventing context bloat
/// and prompt-cache invalidation. Delegates to the tag-generic
/// [`strip_reasoning_tags`] with the reserved default tag pair.
fn strip_think_tags(content: &str) -> String {
    strip_reasoning_tags(content, concat!("<th", "ink>"), concat!("</th", "ink>"))
}

/// Generic reasoning-tag stripper: removes `open_tag`..`close_tag` spans from
/// `content` and trims a dangling open tag.
fn strip_reasoning_tags(content: &str, open_tag: &str, close_tag: &str) -> String {
    let mut result = content.to_string();
    while let Some(start) = result.find(open_tag) {
        let abs = start;
        if let Some(rel_end) = result[abs..].find(close_tag) {
            result.replace_range(abs..abs + rel_end + close_tag.len(), "");
        } else {
            result.truncate(abs);
            break;
        }
    }
    result.trim().to_string()
}

/// Insert a zero-width space after `<` in any occurrence of `open_tag` or
/// `close_tag` inside `text`, so downstream tool-call parsers never see a
/// valid XML-like delimiter embedded in reasoning content.
fn sanitize_tool_delimiters(text: &str, open_tag: &str, close_tag: &str) -> String {
    let mut result = text.to_string();
    for tag in [open_tag, close_tag] {
        // Insert a zero-width space immediately after the `<` of every
        // occurrence of `tag`, so the byte sequence no longer reads as a valid
        // XML-like delimiter. We restart the search from the insertion point so
        // repeated occurrences are all handled in one pass.
        let mut from = 0;
        while from <= result.len() {
            match result[from..].find(tag) {
                Some(rel) => {
                    let pos = from + rel; // index of the tag's opening '<'
                    result.insert(pos + 1, '\u{200b}');
                    // Resume just after the (now length+1) sanitized tag region.
                    from = pos + 1 + tag.len();
                }
                None => break,
            }
        }
    }
    result
}

/// True when the model closed its reasoning block (emitted `close_tag`) and
/// still has non-empty text after the *last* `close_tag` — i.e. a "severed"
/// stop where deliberation ended but no tool call followed. Returns false
/// when `close_tag` is absent (no closed reasoning block to sever on).
fn is_severed_stop(content: &str, close_tag: &str) -> bool {
    match content.rfind(close_tag) {
        Some(pos) => !content[pos + close_tag.len()..].trim().is_empty(),
        None => false,
    }
}

/// The per-turn `metrics` object embedded in a `dispatch` event, in the same
/// camelCase shape the telemetry projection (`parse_session_events`) reads
/// (`prompt_tokens` / `completion_tokens` / `reasoning_tokens`).
///
/// Only the engine-reported token counts are emitted, so an absent field is
/// omitted rather than zero-filled (honest telemetry: `None` never masquerades
/// as a measured zero).
fn metrics_json(m: &Metrics) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    if let Some(p) = m.prompt_tokens {
        o.insert("prompt_tokens".into(), serde_json::json!(p));
    }
    if let Some(k) = m.cached_tokens {
        o.insert("cached_tokens".into(), serde_json::json!(k));
    }
    if let Some(c) = m.completion_tokens {
        o.insert("completion_tokens".into(), serde_json::json!(c));
    }
    if let Some(r) = m.reasoning_tokens {
        o.insert("reasoning_tokens".into(), serde_json::json!(r));
    }
    if let Some(r) = m.tokens_per_sec {
        o.insert("tokens_per_sec".into(), serde_json::json!(r));
    }
    if let Some(t) = m.ttft_ms {
        o.insert("ttft_ms".into(), serde_json::json!(t));
    }
    o.insert("total_ms".into(), serde_json::json!(m.total_ms));
    serde_json::Value::Object(o)
}

/// Per-turn slot-bookkeeping context injected into [`SessionOptions`] (Issues
/// #23 / #28).
///
/// It carries everything the runner's turn loop needs to record a turn's token
/// footprint on a held lease and to participate in the pool's cooperative
/// yield gate:
///
/// - `sem`: the shared token-budgeted slot pool.
/// - `lease`: the currently-held slot lease, wrapped in an `Arc<Mutex<..>>` so
///   the runner can atomically swap it when a yield hands the slot to another
///   worker (the worker holds the same `Arc` to release the final lease on
///   exit).
/// - `task_id`: the task bound to the lease (key for
///   [`crate::task::semaphore::TaskSemaphore::update_context_tokens`]).
/// - `min_eviction_detect_tokens` / `cache_hit_yield_ratio`: the yield-gate
///   thresholds, injected from [`crate::config::Config`] so the runner stays
///   free of any config dependency (mirrors [`should_yield`]).
#[derive(Clone)]
pub struct LeaseYieldContext {
    pub sem: std::sync::Arc<crate::task::semaphore::TaskSemaphore>,
    pub lease: std::sync::Arc<std::sync::Mutex<crate::task::semaphore::SlotLease>>,
    pub task_id: String,
    pub min_eviction_detect_tokens: u32,
    pub cache_hit_yield_ratio: f64,
}

impl std::fmt::Debug for LeaseYieldContext {
    /// `sem` and `lease` hold non-`Debug` handles (`oneshot::Sender` inside the
    /// wait-queue; the slot lease file path), so we print only the identity
    /// and the yield-gate thresholds — enough to log a yield decision without
    /// pulling in the whole semaphore state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseYieldContext")
            .field("task_id", &self.task_id)
            .field(
                "min_eviction_detect_tokens",
                &self.min_eviction_detect_tokens,
            )
            .field("cache_hit_yield_ratio", &self.cache_hit_yield_ratio)
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
    /// The engine's max context (in tokens). Drives the session-history
    /// watermark: a resumed session rolls to a fresh `[system, user]` pair
    /// when the loaded history plus the new prompt is expected to exceed
    /// 75% of this budget (tokens approximated as `chars / 4`). `0` (the
    /// default) means the watermark is disabled — history always resumes
    /// when present (zero behavior change for callers that don't set it).
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
    /// reasoning effort (Issue #27 Part 2). `None` disables the timeout.
    pub turn_timeout_secs: Option<u64>,
    /// Per-turn watchdog reasoning-token budget. When `Some(n)` with
    /// `n > 0`, a turn that returns `finish_reason == "length"` with empty
    /// `tool_calls` and `metrics.reasoning_tokens >= n` is treated as a
    /// stall and retried once at `"low"` reasoning effort (Issue #27 Part 2).
    /// `None` or `0` disables the token-budget trigger.
    pub max_turn_reasoning_tokens: Option<u32>,
    /// Issue #15: maximum bytes of a single tool output before it is spilled
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

    /// Attach the per-turn stall watchdog (Issue #27 Part 2). Wraps each
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
    fn effective_open_tag(&self) -> String {
        self.reasoning_open_tag
            .clone()
            .unwrap_or_else(|| concat!("<th", "ink>").to_string())
    }

    /// Resolve the effective close tag: the configured value when present,
    /// otherwise the reserved default built via `concat!`.
    fn effective_close_tag(&self) -> String {
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

/// Compute the salvage-report path for a session:
/// `<state_or_workspace>/.scratch/salvage_<session_id>.md`.
///
/// Returns `None` when there is no usable base directory — the caller then
/// logs the salvage outcome without writing a file (no crash).
fn salvage_report_path(options: Option<&SessionOptions>, session_id: &str) -> Option<PathBuf> {
    let base = options?.scratch_root()?;
    Some(
        base.join(".scratch")
            .join(format!("salvage_{session_id}.md")),
    )
}

/// Render the full salvage report (header + the model's plain-text summary)
/// to a Markdown file, creating the `.scratch/` directory on demand.
///
/// Never panics on I/O: a failure to create the directory or write the file is
/// returned as an error so the caller can log it cleanly.
fn write_salvage_report(
    path: &Path,
    session_id: &str,
    turns: u32,
    body: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let report = format!(
        "# Salvage report — session `{session_id}`\n\n\
         - **status:** `reasoning_budget_exhausted`\n\
         - **turns consumed:** {turns}\n\n\
         The model reached its reasoning ceiling before completing the task. The \
         conversation history is preserved in the session event ledger; the plain-text \
         summary below was extracted in a single non-coercive salvage turn (tools \
         disabled, low effort).\n\n---\n\n{body}\n"
    );
    fs::write(path, report)
}

/// Default maximum tool result bytes before overflowing to disk.
///
/// Issue #15: this is the fallback threshold used when
/// [`SessionOptions::tool_spill_bytes`] is `None`. It is intentionally
/// conservative (16 KiB) so the default conversation stays small — keeping the
/// Turn-1 prefix byte-for-byte stable across sessions in the same workspace
/// (a warm KV prefix that lets 96%+ of the prefix cache hit). Operators can
/// raise it via `CASTOR_TOOL_SPILL_BYTES`.
pub const SPILL_THRESHOLD_BYTES: usize = 16384;

/// Spill oversized tool outputs to `<scratch>/.scratch/tool_out_<id>.txt` and return
/// a concise pointer block.
///
/// `threshold` is the byte ceiling that triggers a spill; `None` falls back to
/// [`SPILL_THRESHOLD_BYTES`].
pub fn spill_tool_output(
    output: &str,
    tool_call_id: &str,
    scratch_root: Option<&Path>,
    threshold: Option<usize>,
) -> (String, Option<(usize, PathBuf)>) {
    let threshold = threshold.unwrap_or(SPILL_THRESHOLD_BYTES);
    let bytes = output.len();
    if bytes <= threshold {
        return (output.to_string(), None);
    }

    let safe_id: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let safe_id = if safe_id.is_empty() {
        "out".to_string()
    } else {
        safe_id
    };

    let fallback_scratch = std::env::temp_dir().join("castor_scratch");
    let base = scratch_root.unwrap_or(&fallback_scratch);
    let scratch_dir = base.join(".scratch");
    let _ = fs::create_dir_all(&scratch_dir);
    let _ =
        crate::pruner::prune_scratch_dir(&scratch_dir, crate::pruner::DEFAULT_SCRATCH_TTL_MS, None);
    let file_path = scratch_dir.join(format!("tool_out_{safe_id}.txt"));

    match fs::write(&file_path, output) {
        Ok(()) => {
            let head: String = output.chars().take(1024).collect();
            let tail: String = output
                .chars()
                .rev()
                .take(1024)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let elided = bytes.saturating_sub(head.len() + tail.len());
            let pointer = format!(
                "[Tool output spilled to disk: {bytes} bytes > {threshold}-byte threshold. \
Full payload saved to: {}]\n\n\
--- Head preview (first 1024 bytes) ---\n{head}\n\
... [{elided} bytes elided] ...\n\
--- Tail preview (last 1024 bytes) ---\n{tail}\n\n\
Hint: use read_file with start_line/end_line or search_code to inspect specific regions of {}.",
                file_path.display(),
                file_path.display()
            );
            (pointer, Some((bytes, file_path)))
        }
        Err(e) => {
            let head: String = output.chars().take(2048).collect();
            let truncated = format!(
                "[ToolOutputSpillError: could not write to {}: {e}. Payload truncated from {bytes} bytes]\n\n{head}\n\n... [remaining {elided} bytes truncated] ...",
                file_path.display(),
                elided = bytes.saturating_sub(head.len())
            );
            (truncated, None)
        }
    }
}

/// Maximum number of top-level entries emitted in a [`build_workspace_skeleton`]
/// block. Capping keeps the skeleton a tiny, stable prefix even for huge
/// workspaces.
const WORKSPACE_SKELETON_MAX_ENTRIES: usize = 40;

/// Directories that never help a Turn-1 orientation and whose presence would
/// only make the skeleton depend on build state — always excluded so the block
/// stays byte-for-byte stable for a given source tree.
const WORKSPACE_SKELETON_IGNORED: &[&str] = &[".git", "target", "node_modules", ".scratch"];

/// Fixed allowlist of the canonical top-level manifest / contract files the
/// skeleton may surface. Any *other* top-level file is ignored so that
/// arbitrary untracked artifacts (e.g. `random_output.mp4`) never leak into the
/// prefix, keeping it byte-for-byte stable for a given source tree.
const WORKSPACE_SKELETON_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    "README.md",
];

/// Issue #15: build a compact, deterministic `[Workspace Skeleton]` block.
///
/// The block is intentionally tiny and *pure* (a function of the workspace's
/// stable top-level shape alone — no timestamps, no mtimes, no sizes) so that
/// every session started in the same workspace prepends an identical prefix to
/// its system prompt. A byte-identical prefix is what lets 96%+ of the KV cache
/// hit on Turn 1 across concurrent sessions.
///
/// Rules (Finding 4 — hardening against untracked-artifact drift):
/// - **Directories**: every *non-hidden* top-level directory is listed with a
///   trailing `/` (e.g. `src/`). Hidden dirs (any `.` prefix) and the build
///   dirs in [`WORKSPACE_SKELETON_IGNORED`] are excluded.
/// - **Files**: only the canonical manifest / contract files in
///   [`WORKSPACE_SKELETON_MANIFESTS`] are listed. Every other top-level file
///   (including arbitrary untracked output) is dropped.
/// - Each section is sorted lexicographically, so the order is independent of
///   the OS directory-enumeration order; directories come first, then the
///   canonical manifests.
/// - The directory section is capped at [`WORKSPACE_SKELETON_MAX_ENTRIES`]; a
///   trailing `(N more…)` marker is emitted when it is truncated.
/// - If the directory cannot be read, an empty skeleton (header only) is
///   returned so the prefix is still well-defined (deterministic, never a
///   crash) and still identical for the same workspace.
pub fn build_workspace_skeleton(workspace: &std::path::Path) -> String {
    let mut out = String::from("[Workspace Skeleton]\n");
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    if let Ok(iter) = std::fs::read_dir(workspace) {
        for entry in iter.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_hidden = name.starts_with('.');
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                // Keep only stable, non-hidden source dirs; drop hidden and
                // build-state dirs (`.git`, `.scratch`, `target`, …).
                if !is_hidden && !WORKSPACE_SKELETON_IGNORED.contains(&name.as_str()) {
                    dirs.push(name);
                }
            } else if WORKSPACE_SKELETON_MANIFESTS.contains(&name.as_str()) {
                files.push(name);
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    files.sort();
    files.dedup();
    // Deterministic ordering: non-hidden source dirs (trailing `/`) first,
    // then the canonical manifest / contract files. Both sections are
    // independently sorted, so the whole block is a pure function of the
    // workspace's stable top-level shape.
    for d in dirs.iter().take(WORKSPACE_SKELETON_MAX_ENTRIES) {
        out.push_str(&format!("- {d}/\n"));
    }
    let more_dirs = dirs.len().saturating_sub(WORKSPACE_SKELETON_MAX_ENTRIES);
    for f in &files {
        out.push_str(&format!("- {f}\n"));
    }
    if more_dirs > 0 {
        out.push_str(&format!("({more_dirs} more…)\n"));
    }
    out
}

/// The on-disk shape of a single persisted history message.
///
/// Deliberately a *projection* of [`Message`]: it carries only `role`,
/// `content`, `tool_calls`, and `tool_call_id` — it never carries a
/// `reasoning_content` payload, so persisted history is sanitized of any
/// deliberation trace ("cleared to `None`").
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedMessage {
    role: String,
    content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<PersistedToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedToolCall {
    index: usize,
    id: String,
    name: String,
    arguments: String,
}

impl From<&PersistedMessage> for Message {
    fn from(p: &PersistedMessage) -> Self {
        Message {
            role: p.role.clone(),
            content: p.content.clone(),
            tool_calls: p
                .tool_calls
                .iter()
                .map(|t| ToolCall {
                    index: t.index,
                    id: t.id.clone(),
                    name: t.name.clone(),
                    arguments: t.arguments.clone(),
                })
                .collect(),
            tool_call_id: p.tool_call_id.clone(),
        }
    }
}

/// Project a [`Message`] into its persisted (sanitized) form.
fn to_persisted(m: &Message) -> PersistedMessage {
    // Reasoning is never persisted: the projection has no `reasoning_content`
    // field at all (the "cleared to None" guarantee), and any ` think` trace
    // inside assistant content is stripped to prevent context bloat.
    let content = if m.role == "assistant" {
        strip_think_tags(&m.content)
    } else {
        m.content.clone()
    };
    PersistedMessage {
        role: m.role.clone(),
        content,
        tool_calls: m
            .tool_calls
            .iter()
            .map(|t| PersistedToolCall {
                index: t.index,
                id: t.id.clone(),
                name: t.name.clone(),
                arguments: t.arguments.clone(),
            })
            .collect(),
        tool_call_id: m.tool_call_id.clone(),
    }
}

/// The persisted-history location: `<state>/sessions/<session_id>/messages.json`.
fn session_messages_path(state: &StateDir, session_id: &str) -> PathBuf {
    state.sessions().join(session_id).join("messages.json")
}

/// Persist a session's sanitized conversation history.
///
/// Drops `system` messages (the system prompt is re-supplied on resume, not
/// stored) and strips ` think` traces from assistant messages. Best-effort: a
/// write failure is logged, never propagated (it must not crash the session).
fn save_messages(state: &StateDir, session_id: &str, messages: &[Message]) {
    let path = session_messages_path(state, session_id);
    let sanitized: Vec<PersistedMessage> = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(to_persisted)
        .collect();
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        warn!(session = %session_id, "failed to create session history dir: {e}");
        return;
    }
    match serde_json::to_vec_pretty(&sanitized) {
        Ok(bytes) => {
            if let Err(e) = fs::write(&path, bytes) {
                warn!(session = %session_id, "failed to persist session history: {e}");
            }
        }
        Err(e) => {
            warn!(session = %session_id, "failed to serialize session history: {e}");
        }
    }
}

/// Load a previously persisted session history.
///
/// Returns `None` when the file is absent or unreadable (a fresh session) or
/// when it cannot be parsed. On success, returns the reconstructed non-system
/// [`Message`]s, ready to be prepended to a fresh `[system, user]` pair.
fn load_messages(state: &StateDir, session_id: &str) -> Option<Vec<Message>> {
    let path = session_messages_path(state, session_id);
    let bytes = fs::read(&path).ok()?;
    let persisted: Vec<PersistedMessage> = serde_json::from_slice(&bytes).ok()?;
    Some(persisted.iter().map(Message::from).collect())
}

/// Approximate a text's token count as `chars / 4` (the rough chars-per-token
/// heuristic for a 4-chars/token packing).
fn approx_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

/// Approximate a message list's token count (sum of per-message `chars / 4`).
fn history_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| approx_tokens(&m.content)).sum()
}

/// The 75% rollover watermark for a given `max_context` token budget.
fn watermark_tokens(max_context: usize) -> usize {
    (max_context as u64 * 75 / 100) as usize
}

/// The token footprint of a *resumed* session's loaded history, subject to the
/// same 75% [`max_context`] watermark gate used by [`run_session`] on resume.
///
/// This is the DRY source of truth for "how much context is this task already
/// carrying into the slot pool" (Issues #23 / #28):
///
/// - No state dir, no persisted history, or a `max_context` that is `None` /
///   `0` (watermark disabled) → returns `0` (nothing to account for, exactly
///   matching the resume path's "disabled" behavior).
/// - The loaded history fits within the 75% watermark → its
///   [`history_tokens`] footprint is returned.
/// - The loaded history exceeds the watermark → `0` (the session would roll
///   over to a fresh `[system, user]` pair, so it carries no resumed history).
pub fn resumed_history_tokens(state: &StateDir, session_id: &str, max_context: Option<u32>) -> u32 {
    let Some(mc) = max_context else {
        return 0;
    };
    if mc == 0 {
        return 0; // watermark disabled
    }
    let Some(history) = load_messages(state, session_id) else {
        return 0;
    };
    if history.is_empty() {
        return 0;
    }
    let wm = watermark_tokens(mc as usize);
    if history_tokens(&history) <= wm {
        history_tokens(&history) as u32
    } else {
        0 // over the watermark → would roll over → no resumed history
    }
}

/// Decide whether a loaded history should be resumed or rolled over.
///
/// Resumes when the watermark is disabled (no options, or `max_context == 0`)
/// or when the loaded history plus the new user prompt fits within 75% of
/// `max_context`. Otherwise the session rolls over to a fresh `[system, user]`
/// pair (the caller simply does not prepend the history).
fn should_resume(
    history: &[Message],
    new_user_prompt: &str,
    options: Option<&SessionOptions>,
) -> bool {
    let Some(opts) = options else {
        return true; // no options → no budget known → resume (disabled)
    };
    if opts.max_context == 0 {
        return true; // watermark disabled
    }
    let wm = watermark_tokens(opts.max_context);
    let est = history_tokens(history) + approx_tokens(new_user_prompt);
    est <= wm
}

/// Per-turn stall-watchdog wrapper for a single [`ChatEngine::chat`] call
/// (Issue #27 Part 2).
///
/// Two independent triggers, both optional (`None` / `0` disables that
/// trigger):
///
/// - **Wall-clock timeout**: when `turn_timeout_secs > 0`, the chat call is
///   wrapped in [`tokio::time::timeout`]. A timeout is a hard stall.
/// - **Reasoning-token budget**: when `max_turn_reasoning_tokens > 0` and the
///   turn returns `finish_reason == "length"` with no `tool_calls` and
///   `metrics.reasoning_tokens` at or above the budget, the model is presumed
///   to have reasoned its way to a dead end (it "stalled" in deliberation
///   rather than acting).
///
/// When either trigger fires, a `"turn_stall_retry"` event is appended to
/// `logger` and the chat is retried **once** at `"low"` reasoning effort — a
/// cheap, deterministic nudge that breaks the deliberation loop without
/// changing the session's effective tier. If the retry itself errors, that
/// error is returned. When no trigger fires, the original completion is
/// returned untouched.
#[allow(clippy::too_many_arguments)] // each argument is a distinct watchdog input
async fn chat_with_watchdog(
    engine: &dyn ChatEngine,
    messages: &[Message],
    tools: &[ToolSchema],
    effort: Option<&str>,
    turn_timeout_secs: u64,
    max_turn_reasoning_tokens: u32,
    logger: &EventLogger,
    turn: u32,
) -> Result<crate::engine::Completion, EngineError> {
    // 1. Issue the primary call, optionally under a wall-clock timeout.
    let first = if turn_timeout_secs > 0 {
        match tokio::time::timeout(
            std::time::Duration::from_secs(turn_timeout_secs),
            engine.chat(messages, tools, true, effort),
        )
        .await
        {
            Ok(r) => r,
            Err(_elapsed) => {
                // Timed out: the in-flight request is dropped on timeout.
                let _ = logger.append(serde_json::json!({
                    "type": "turn_stall_retry",
                    "turn": turn,
                    "reason": "timeout",
                    "turn_timeout_secs": turn_timeout_secs,
                    "retry_reasoning_effort": "low",
                }));
                tracing::warn!(
                    turn = turn,
                    "turn stalled on wall-clock timeout ({}s); retrying once at 'low' effort",
                    turn_timeout_secs
                );
                return engine.chat(messages, tools, true, Some("low")).await;
            }
        }
    } else {
        engine.chat(messages, tools, true, effort).await
    };

    let Ok(completion) = first else {
        // Non-timeout error: nothing to salvage; propagate as-is.
        return first;
    };

    // 2. Reasoning-token budget stall: `length` finish, no tool calls, and the
    //    model burned at least the budget in reasoning tokens.
    let stalled = max_turn_reasoning_tokens > 0
        && completion.finish_reason.as_deref() == Some("length")
        && completion.tool_calls.is_empty()
        && completion.metrics.reasoning_tokens.unwrap_or(0) >= u64::from(max_turn_reasoning_tokens);

    if stalled {
        let _ = logger.append(serde_json::json!({
            "type": "turn_stall_retry",
            "turn": turn,
            "reason": "reasoning_token_budget",
            "reasoning_tokens": completion.metrics.reasoning_tokens,
            "max_turn_reasoning_tokens": max_turn_reasoning_tokens,
            "retry_reasoning_effort": "low",
        }));
        tracing::warn!(
            turn = turn,
            reasoning_tokens = completion.metrics.reasoning_tokens,
            "turn stalled at reasoning-token budget ({}); retrying once at 'low' effort",
            max_turn_reasoning_tokens
        );
        return engine.chat(messages, tools, true, Some("low")).await;
    }

    Ok(completion)
}

/// Run a multi-turn agent session.
///
/// Builds the message list (tool schemas last, for the vLLM APC contract),
/// calls the engine, executes any tool calls via the [`ToolExecutor`], appends
/// the results as tool messages, and continues until the model produces a
/// final or the turn budget is exhausted.
///
/// `options` supplies the state / workspace context the runner needs for
/// terminal artifacts. It is optional: pass `None` for callers that only
/// care about the in-band [`SessionResult`] (e.g. the offline eval replay).
/// When present and the session terminates at its reasoning ceiling, the
/// salvage pass writes a plain-text report to
/// `<state_or_workspace>/.scratch/salvage_<session_id>.md` (see
/// [`SessionOptions`] and the deliberation-ceiling salvage path below).
#[allow(clippy::too_many_arguments)] // each argument is a distinct session dependency
pub async fn run_session(
    engine: &dyn ChatEngine,
    executor: &dyn ToolExecutor,
    logger: &EventLogger,
    system_prompt: &str,
    user_prompt: &str,
    tools: &[ToolSchema],
    turns_budget: u32,
    options: Option<&SessionOptions>,
) -> Result<SessionResult, RunnerError> {
    let budget = if turns_budget == 0 {
        DEFAULT_TURNS_BUDGET
    } else {
        turns_budget
    };
    let session_id = logger.session_id();

    // Wall-clock start: the `final` event reports `duration_ms` (the total
    // session runtime) so the telemetry duration series has a source.
    let started = std::time::Instant::now();

    // Named sessions resume their persisted history (see the module docs):
    // the system prompt is re-supplied first, the sanitized non-system history
    // is prepended (if present and within the 75% `max_context` watermark), and
    // the new user prompt is appended last. `None` state or an empty/absent
    // history yields the original fresh `[system, user]` shape.
    let mut messages = vec![msg("system", system_prompt)];
    if let Some(state) = options.and_then(|o| o.state.as_ref())
        && let Some(history) = load_messages(state, session_id)
        && !history.is_empty()
    {
        if should_resume(&history, user_prompt, options) {
            messages.extend(history);
        } else {
            tracing::info!(
                session = %session_id,
                "persisted history exceeds the 75% max_context watermark; \
                 rolling over to a fresh session"
            );
        }
    }
    messages.push(msg("user", user_prompt));
    let mut loop_detector = LoopDetector::new(LOOP_WINDOW, LOOP_THRESHOLD);
    let mut turns: u32 = 0;
    let mut final_text = String::new();
    let mut final_produced = false;
    let mut tool_activity = false;
    let mut landing_injected = false;
    let mut status = "completed".to_string();

    // Per-session reasoning-effort override, applied uniformly to every
    // engine call in the session (main turns, salvage, synthesis) — a
    // mid-session tier change would break the prefix cache, so the tier
    // is session-scoped. `None` leaves the payload byte-identical to the
    // no-override shape (the server-side default applies).
    let effort = options
        .and_then(|o| o.reasoning_effort.as_deref())
        .filter(|e| !e.is_empty());

    // Per-turn stall watchdog (Issue #27 Part 2). `0` disables each trigger;
    // both default to disabled so callers without a watchdog are unchanged.
    let (wd_timeout, wd_max_reasoning) = match options {
        Some(o) => (
            o.turn_timeout_secs.unwrap_or(0),
            o.max_turn_reasoning_tokens.unwrap_or(0),
        ),
        None => (0, 0),
    };

    while turns < budget {
        // Cooperative landing: in the last ~5 turns, inject a budget notice.
        let remaining = budget - turns;
        if remaining <= LANDING_WINDOW && !landing_injected {
            let notice = format!(
                "[Budget Notice] You have {remaining} turns left in your budget. Land now: \
                 synthesize your final deliverable, findings, and grounded conclusions. \
                 Do not start new tool calls."
            );
            messages.push(msg("user", notice));
            landing_injected = true;
        }

        turns += 1;

        // Build the request: messages first, tool schemas last (APC contract).
        // The per-turn stall watchdog (Issue #27 Part 2) wraps the call when a
        // timeout or reasoning-token budget is configured; otherwise the call
        // is issued directly (zero behavior change for unconfigured callers).
        let watchdog = wd_timeout > 0 || wd_max_reasoning > 0;
        let completion = if watchdog {
            let primary = chat_with_watchdog(
                engine,
                &messages,
                tools,
                effort,
                wd_timeout,
                wd_max_reasoning,
                logger,
                turns,
            )
            .await;
            match primary {
                Ok(c) => c,
                Err(crate::engine::EngineError::Sse(err)) => {
                    tracing::warn!(
                        "Transient SSE stream error on turn {turns}: {err}; retrying once"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    engine.chat(&messages, tools, true, effort).await?
                }
                Err(err) => return Err(RunnerError::Engine(err)),
            }
        } else {
            match engine.chat(&messages, tools, true, effort).await {
                Ok(c) => c,
                Err(crate::engine::EngineError::Sse(err)) => {
                    tracing::warn!(
                        "Transient SSE stream error on turn {turns}: {err}; retrying once"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    engine.chat(&messages, tools, true, effort).await?
                }
                Err(err) => return Err(RunnerError::Engine(err)),
            }
        };

        // Record the dispatch event (with the engine's per-turn token usage,
        // so current Rust runs contribute to the telemetry token totals).
        logger
            .append(serde_json::json!({
                "type": "dispatch",
                "turn": turns,
                "finish_reason": completion.finish_reason,
                "content": completion.content,
                "tool_calls": completion
                    .tool_calls
                    .iter()
                    .map(|tc| serde_json::json!({
                        "id": tc.id,
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }))
                    .collect::<Vec<_>>(),
                "metrics": metrics_json(&completion.metrics),
            }))
            .map_err(RunnerError::Io)?;

        // Append the assistant message. Strip <th_ink>...</th_ink> traces if tool_calls
        // were made, to prevent context inflation and prompt-cache invalidation.
        let sanitized_content = if !completion.tool_calls.is_empty() {
            strip_think_tags(&completion.content)
        } else {
            completion.content.clone()
        };
        messages.push(Message {
            role: "assistant".into(),
            content: sanitized_content,
            tool_calls: completion.tool_calls.clone(),
            tool_call_id: None,
        });

        if completion.tool_calls.is_empty() {
            // Compute the ceiling signal before moving `completion.content`.
            let ceiling_hit = is_reasoning_ceiling(&completion);

            // Issue #24: capture the severed-stop signal (reasoning closed, but
            // the model stopped with no tool call and no real content) before
            // moving `completion`. This triggers a one-time continuation prompt
            // so the engine is given a chance to act after deliberating.
            let close_tag = options
                .map(|o| o.effective_close_tag())
                .unwrap_or_else(|| concat!("</th", "ink>").to_string());
            let severed_stop = completion
                .finish_reason
                .as_deref()
                .map(|f| f == "stop")
                .unwrap_or(false)
                && completion.reasoning_content.is_some()
                && is_severed_stop(&completion.content, &close_tag);

            // Final content: if content is empty but the model provided substantive
            // reasoning_content (common when reasoning models finish deliberation
            // or emit conclusions in thought tokens), preserve the reasoning trace
            // as the final output rather than discarding it.
            final_text = if !completion.content.trim().is_empty() {
                completion.content
            } else if let Some(rc) = completion
                .reasoning_content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                rc.to_string()
            } else {
                completion.content
            };
            final_produced = true;

            // One-time continuation: the model deliberated (closed its reasoning
            // block) but stopped without a tool call and without a real deliverable.
            // Prompt it once to continue acting; if it then emits tool calls, the
            // loop continues naturally — otherwise we land on the new content.
            if severed_stop {
                let continuation_prompt = concat!(
                    "[Continuation] Your last turn closed with ",
                    "th_ink",
                    " reasoning but produced no tool call or final deliverable. \
                     If you still need to take an action (read, write, run a tool, \
                     or synthesize a final answer), do so now. If the task is \
                     already complete, reply with your final answer."
                );
                messages.push(msg("user", continuation_prompt));
                match engine.chat(&messages, tools, true, effort).await {
                    Ok(cont) => {
                        if cont.tool_calls.is_empty() {
                            // Terminal: land on the continuation's content (or its
                            // reasoning if the content is empty).
                            final_text = if !cont.content.trim().is_empty() {
                                cont.content
                            } else if let Some(rc) = cont
                                .reasoning_content
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                rc.to_string()
                            } else {
                                cont.content
                            };
                            final_produced = !final_text.trim().is_empty() || final_produced;
                            break;
                        }
                        // The model acted: append the assistant message with its
                        // tool calls so the loop continues and executes them.
                        messages.push(Message {
                            role: "assistant".into(),
                            content: cont.content,
                            tool_calls: cont.tool_calls.clone(),
                            tool_call_id: None,
                        });
                        let scratch_root = options.and_then(|o| o.scratch_root());
                        let spill_threshold = options.and_then(|o| o.tool_spill_bytes);
                        for tc in &cont.tool_calls {
                            let outcome = match executor.execute(&tc.name, &tc.arguments).await {
                                Ok(o) => o,
                                Err(e) => ToolOutcome {
                                    text: format!("Error: {e}"),
                                },
                            };
                            tool_activity = true;
                            let (tool_content, spilled) = spill_tool_output(
                                &outcome.text,
                                &tc.id,
                                scratch_root.as_deref(),
                                spill_threshold,
                            );
                            if let Some((bytes, path)) = spilled {
                                let _ = logger.append(serde_json::json!({
                                    "type": "tool_output_spilled",
                                    "sessionId": session_id,
                                    "toolCallId": tc.id,
                                    "toolName": tc.name,
                                    "bytes": bytes,
                                    "path": path.to_string_lossy(),
                                }));
                            }
                            let _ = logger.append(serde_json::json!({
                                "type": "tool_result",
                                "turn": turns,
                                "tool_call_id": tc.id,
                                "name": tc.name,
                                "is_error": outcome.text.starts_with("Error:"),
                                "output": tool_content,
                            }));
                            messages.push(Message {
                                role: "tool".into(),
                                content: tool_content,
                                tool_call_id: Some(tc.id.clone()),
                                tool_calls: Vec::new(),
                            });
                        }
                        // Loop continues: `turns` was already incremented for this
                        // iteration, so the next iteration consumes a fresh budget
                        // unit for the follow-up assistant turn.
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            session = %session_id,
                            "continuation call after severed stop failed: {e}; \
                             landing on prior content"
                        );
                        break;
                    }
                }
            }

            // Deliberation-ceiling salvage pass: the model stopped at its
            // reasoning ceiling on a non-tool turn. The honest status is kept,
            // the accumulated conversation history is preserved, the peer
            // engineer is empowered with full tools to inspect scratchpads or
            // verify test outputs, and a grounded status report is extracted.
            if ceiling_hit {
                status = "reasoning_budget_exhausted".to_string();

                // 1. History is preserved: the full `messages` (system prompt,
                //    user prompt, every assistant turn and tool result so far)
                //    is passed intact to the salvage call — never cleared.
                // 2. Inject the non-coercive, peer-empowered salvage prompt.
                messages.push(msg("user", SALVAGE_PROMPT));
                // 3. Call the engine with full tools enabled to give the peer
                //    engineer full agency to inspect scratchpad files or check
                //    logs before reporting.
                let salvage = engine.chat(&messages, tools, false, effort).await;
                match salvage {
                    Ok(mut sc) => {
                        // If the peer engineer executed tools during salvage,
                        // run each tool call, log results, append them to history,
                        // and perform a single-shot synthesis call.
                        if !sc.tool_calls.is_empty() {
                            messages.push(Message {
                                role: "assistant".into(),
                                content: sc.content.clone(),
                                tool_calls: sc.tool_calls.clone(),
                                tool_call_id: None,
                            });
                            for tc in &sc.tool_calls {
                                let outcome = match executor.execute(&tc.name, &tc.arguments).await
                                {
                                    Ok(o) => o,
                                    Err(e) => ToolOutcome {
                                        text: format!("Error: {e}"),
                                    },
                                };
                                let scratch_root = options.and_then(|o| o.scratch_root());
                                let spill_threshold = options.and_then(|o| o.tool_spill_bytes);
                                let (tool_content, spilled) = spill_tool_output(
                                    &outcome.text,
                                    &tc.id,
                                    scratch_root.as_deref(),
                                    spill_threshold,
                                );
                                if let Some((bytes, path)) = spilled {
                                    let _ = logger.append(serde_json::json!({
                                        "type": "tool_output_spilled",
                                        "sessionId": session_id,
                                        "toolCallId": tc.id,
                                        "toolName": tc.name,
                                        "bytes": bytes,
                                        "path": path.to_string_lossy(),
                                    }));
                                }
                                let _ = logger.append(serde_json::json!({
                                    "type": "tool_result",
                                    "turn": turns + 1,
                                    "tool_call_id": tc.id,
                                    "name": tc.name,
                                    "is_error": outcome.text.starts_with("Error:"),
                                    "output": tool_content,
                                }));
                                messages.push(Message {
                                    role: "tool".into(),
                                    content: tool_content,
                                    tool_calls: vec![],
                                    tool_call_id: Some(tc.id.clone()),
                                });
                            }
                            // Single follow-up request to synthesize findings based on tool outputs.
                            match engine.chat(&messages, &[], false, effort).await {
                                Ok(follow_up) => {
                                    sc = follow_up;
                                }
                                Err(e) => {
                                    warn!(session = %session_id, "salvage tool follow-up synthesis failed: {e}");
                                }
                            }
                        }

                        // Ledger the salvage turn (history stays in `messages`).
                        let _ = logger.append(serde_json::json!({
                            "type": "salvage",
                            "turn": turns + 1,
                            "reason": "reasoning_ceiling",
                            "content": sc.content,
                        }));
                        let trimmed = sc.content.trim();
                        if trimmed.is_empty() {
                            // Salvage produced no text: honest status, clean log,
                            // no crash, and the prior turn's content (if any) is
                            // kept as the in-band note.
                            warn!(
                                session = %session_id,
                                "salvage produced no content; preserving honest status \
                                 reasoning_budget_exhausted"
                            );
                            final_text = "The session terminated at its reasoning ceiling \
                                         (reasoning_budget_exhausted). The salvage pass returned \
                                         no content, so no summary is available; the conversation \
                                         history is preserved in the session event ledger."
                                .to_string();
                        } else {
                            // 4. Persist the salvage report to .scratch/.
                            let written_path = match salvage_report_path(options, session_id) {
                                Some(p) => {
                                    match write_salvage_report(&p, session_id, turns, trimmed) {
                                        Ok(()) => Some(p),
                                        Err(e) => {
                                            warn!(
                                                session = %session_id,
                                                "failed to write salvage report: {e}"
                                            );
                                            None
                                        }
                                    }
                                }
                                None => {
                                    warn!(
                                        session = %session_id,
                                        "no state/workspace available for the salvage report"
                                    );
                                    None
                                }
                            };
                            let path_note = match written_path {
                                Some(p) => format!(
                                    "Salvage report written to: {}\n\nSalvage summary:\n{}",
                                    p.display(),
                                    trimmed
                                ),
                                None => format!("Salvage summary:\n{trimmed}"),
                            };
                            // 5. Honest status is never masked as success; the
                            //    salvage path is annotated in final_text.
                            final_text = format!(
                                "The session terminated at its reasoning ceiling \
                                 (status: reasoning_budget_exhausted); a salvage pass recovered the \
                                 summary below.\n\n{path_note}"
                            );
                        }
                    }
                    Err(e) => {
                        // 6. Salvage call failed: log cleanly, never crash, and
                        //    keep the honest status.
                        warn!(
                            session = %session_id,
                            "salvage call failed: {e}; preserving honest status \
                             reasoning_budget_exhausted"
                        );
                        let _ = logger.append(serde_json::json!({
                            "type": "salvage",
                            "turn": turns + 1,
                            "reason": "reasoning_ceiling",
                            "error": e.to_string(),
                        }));
                        final_text = format!(
                            "The session terminated at its reasoning ceiling \
                             (status: reasoning_budget_exhausted); the salvage pass failed ({e}), \
                             so no summary is available. The conversation history is preserved in \
                             the session event ledger."
                        );
                    }
                }
                break;
            }

            // Degenerate final: empty/whitespace after real tool activity.
            if final_text.trim().is_empty() && tool_activity {
                // One salvage retry asking for a proper summary.
                messages.push(msg(
                    "user",
                    "[Salvage] Your previous response was empty. Provide a proper summary of \
                     your findings and the work you have completed.",
                ));
                let salvage = engine.chat(&messages, &[], true, effort).await?;
                let salvage_text = if !salvage.content.trim().is_empty() {
                    salvage.content
                } else if let Some(rc) = salvage
                    .reasoning_content
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    rc.to_string()
                } else {
                    String::new()
                };
                logger
                    .append(serde_json::json!({
                        "type": "salvage",
                        "turn": turns,
                        "content": salvage_text,
                    }))
                    .map_err(RunnerError::Io)?;
                if !salvage_text.trim().is_empty() {
                    final_text = salvage_text;
                } else {
                    // Honest failure.
                    status = "failed".to_string();
                    final_text = "DegenerateFinalError: the model produced an empty final after \
                                  tool activity; the salvage retry also produced no content."
                        .to_string();
                }
            }

            break;
        }

        // Execute each tool call.
        for tc in &completion.tool_calls {
            let outcome = match executor.execute(&tc.name, &tc.arguments).await {
                Ok(o) => o,
                Err(e) => ToolOutcome {
                    text: format!("Error: {e}"),
                },
            };
            tool_activity = true;

            let (raw_tool_content, image_attachment) =
                if outcome.text.starts_with("[IMAGE_ATTACHMENT:") {
                    if let Some(end_idx) = outcome.text.find(']') {
                        let header = &outcome.text[..end_idx];
                        let rest = outcome.text[end_idx + 1..].trim_start_matches(['\r', '\n']);
                        let data_url = header.strip_prefix("[IMAGE_ATTACHMENT:").unwrap_or("");
                        let data_url = data_url.split(":path:").next().unwrap_or("");
                        (rest.to_string(), Some(data_url.to_string()))
                    } else {
                        (outcome.text.clone(), None)
                    }
                } else {
                    (outcome.text.clone(), None)
                };

            let scratch_root = options.and_then(|o| o.scratch_root());
            let spill_threshold = options.and_then(|o| o.tool_spill_bytes);
            let (tool_content, spilled) = spill_tool_output(
                &raw_tool_content,
                &tc.id,
                scratch_root.as_deref(),
                spill_threshold,
            );
            if let Some((bytes, path)) = spilled {
                let _ = logger.append(serde_json::json!({
                    "type": "tool_output_spilled",
                    "sessionId": session_id,
                    "toolCallId": tc.id,
                    "toolName": tc.name,
                    "bytes": bytes,
                    "path": path.to_string_lossy(),
                }));
            }

            // Append the tool result as a tool message. Sanitize any embedded
            // reasoning delimiters so a downstream tool-call parser never sees a
            // valid `<th_ink>`-style delimiter inside tool output.
            let open_tag = options
                .map(|o| o.effective_open_tag())
                .unwrap_or_else(|| concat!("<th", "ink>").to_string());
            let close_tag = options
                .map(|o| o.effective_close_tag())
                .unwrap_or_else(|| concat!("</th", "ink>").to_string());
            let sanitized_tool_content =
                sanitize_tool_delimiters(&tool_content, &open_tag, &close_tag);
            messages.push(Message {
                role: "tool".into(),
                content: sanitized_tool_content.clone(),
                tool_call_id: Some(tc.id.clone()),
                tool_calls: Vec::new(),
            });

            if let Some(data_url) = image_attachment {
                // The engine carries a 1-image limit: clear any prior visual
                // attachment user messages so multi-turn visual tasks never
                // exceed it, then append the fresh attachment.
                for m in &mut messages {
                    if m.role == "user" && m.content.starts_with("[Visual Inspection Attachment:") {
                        m.content =
                            "[Prior visual attachment cleared to stay within the 1-image limit]"
                                .to_string();
                    }
                }
                messages.push(Message {
                    role: "user".into(),
                    content: format!("[Visual Inspection Attachment: {data_url}]"),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                });
            }

            // Record the tool_result event.
            logger
                .append(serde_json::json!({
                    "type": "tool_result",
                    "turn": turns,
                    "tool_call_id": tc.id,
                    "name": tc.name,
                    "is_error": outcome.text.starts_with("Error:"),
                    "output": tool_content,
                }))
                .map_err(RunnerError::Io)?;

            // Loop detection (catches single-action and multi-action periodic cycles).
            match loop_detector.record(&tc.name, &tc.arguments) {
                LoopState::Ok => {}
                LoopState::Advisory => {
                    let advisory = if tc.name == "read_file" {
                        "[Read Advisory] You have repeated the exact same read on this coordinate \
multiple times. If you are facing contradictory requirements across files or an architectural \
impasse, state your findings and ask for alignment rather than continuing to re-read."
                            .to_string()
                    } else {
                        "[Loop Advisory] You have repeated the same action sequence multiple times in a \
                         row. Change your approach or synthesize your findings."
                            .to_string()
                    };
                    messages.push(msg("user", advisory));
                }
                LoopState::LoopDetected => {
                    return Err(RunnerError::LoopDetected {
                        name: tc.name.clone(),
                    });
                }
            }
        }

        // End of turn: persist the sanitized history so a later dispatch can
        // resume it (subject to the 75% `max_context` watermark on load).
        if let Some(state) = options.and_then(|o| o.state.as_ref()) {
            save_messages(state, session_id, &messages);
        }

        // Issues #23 / #28: per-turn slot bookkeeping + cooperative yield.
        // Only runs when the worker injected a `LeaseYieldContext`. We record
        // the turn's token footprint on the held lease, then let the pool's
        // yield gate decide whether this worker should hand its slot to a
        // younger / more cache-efficient worker. `completion` is still alive
        // here (the next loop iteration re-binds it), so we read its metrics.
        if let Some(ctx) = options.and_then(|o| o.lease_context.as_ref()) {
            let prompt_tokens = completion.metrics.prompt_tokens;
            let completion_tokens = completion.metrics.completion_tokens;
            let cached_tokens = completion.metrics.cached_tokens;
            let turn_tokens = prompt_tokens
                .map(|p| p as u32)
                .unwrap_or_else(|| history_tokens(&messages) as u32)
                .saturating_add(completion_tokens.map(|c| c as u32).unwrap_or(0));
            ctx.sem.update_context_tokens(&ctx.task_id, turn_tokens);
            // Lock scope is deliberately narrowed to the synchronous decision
            // (read lease + decide + snapshot) so no guard is held across the
            // async re-acquire below (Issues #23 / #28, clippy::await_holding_lock).
            let maybe_swap = match ctx.lease.lock() {
                Ok(current) => {
                    if ctx.sem.should_yield(
                        &current,
                        turn_tokens,
                        cached_tokens.map(|c| c as u32),
                        prompt_tokens.map(|p| p as u32),
                        ctx.min_eviction_detect_tokens,
                        ctx.cache_hit_yield_ratio,
                    ) {
                        Some(current.clone())
                    } else {
                        None
                    }
                }
                // Poisoned lock: skip the yield and continue with the turn.
                Err(_) => None,
            };
            if let Some(old) = maybe_swap {
                let _ = logger.append(serde_json::json!({
                    "type": "cooperative_yield",
                    "turn": turns,
                    "task_id": ctx.task_id,
                    "turn_tokens": turn_tokens,
                    "reason": "pool_yield_gate",
                }));
                // True cooperative yield handoff (Issues #23 / #28): release
                // the held slot, wait (up to 30s) until no *older* lease
                // remains so the older worker makes real progress, then
                // re-acquire. This prevents the younger worker from winning its
                // own slot back in the very next FIFO turn.
                let new_lease = ctx
                    .sem
                    .yield_to_older_and_reacquire(
                        &old,
                        &ctx.task_id,
                        turn_tokens,
                        std::time::Duration::from_secs(30),
                    )
                    .await;
                if let Ok(mut slot) = ctx.lease.lock() {
                    *slot = new_lease;
                }
            }
        }
    }

    // Budget exhausted.
    if !final_produced {
        // Best-effort synthesis: ask the model to land one more time (tools stripped).
        messages.push(msg(
            "user",
            "[Final] Your turn budget is exhausted. Synthesize your final deliverable, findings, \
             and grounded conclusions now.",
        ));
        let completion = engine.chat(&messages, &[], true, effort).await?;
        logger
            .append(serde_json::json!({
                "type": "dispatch",
                "turn": turns + 1,
                "finish_reason": completion.finish_reason,
                "content": completion.content,
                "tool_calls": [],
                "metrics": metrics_json(&completion.metrics),
            }))
            .map_err(RunnerError::Io)?;
        final_text = completion.content;
        status = "completed_budget_exhausted".to_string();
    } else if status == "completed" && turns >= budget {
        // The model landed on the last turn (cooperative landing). Only
        // reclassify a plain "completed" result — never a more specific
        // terminal status such as `reasoning_budget_exhausted`, which must
        // never be masked (honest-status invariant).
        status = "completed_budget_exhausted".to_string();
    }

    // Record the final event.
    let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    logger
        .append(serde_json::json!({
            "type": "final",
            "turn": turns,
            "status": status,
            "final_text": final_text,
            "duration_ms": elapsed_ms,
        }))
        .map_err(RunnerError::Io)?;

    // Normal completion (and budget-exhausted landing): persist the final
    // sanitized history so a later dispatch resumes from here.
    if let Some(state) = options.and_then(|o| o.state.as_ref()) {
        save_messages(state, session_id, &messages);
    }

    Ok(SessionResult {
        final_text,
        turns,
        budget,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Completion, Metrics, ToolCall};
    use crate::state::StateDir;
    use std::collections::VecDeque;

    /// One recorded engine call: the message history, the tool schemas, and
    /// the stream flag (the flag lets tests verify the salvage call runs
    /// non-streaming, i.e. the low-effort extraction).
    #[derive(Default, Clone)]
    struct RecordedCall {
        messages: Vec<Message>,
        tools: Vec<ToolSchema>,
        stream: bool,
    }

    /// A recorder that captures engine calls for assertions.
    #[derive(Default)]
    struct CallRecorder {
        calls: tokio::sync::Mutex<Vec<RecordedCall>>,
    }

    /// A scripted engine that records calls via a shared recorder.
    struct RecordingEngine {
        responses: tokio::sync::Mutex<VecDeque<Completion>>,
        recorder: std::sync::Arc<CallRecorder>,
    }

    impl RecordingEngine {
        fn new(responses: Vec<Completion>, recorder: std::sync::Arc<CallRecorder>) -> Self {
            Self {
                responses: tokio::sync::Mutex::new(responses.into()),
                recorder,
            }
        }
    }

    #[async_trait]
    impl ChatEngine for RecordingEngine {
        async fn chat(
            &self,
            messages: &[Message],
            tools: &[ToolSchema],
            stream: bool,
            _reasoning_effort: Option<&str>,
        ) -> Result<Completion, EngineError> {
            self.recorder.calls.lock().await.push(RecordedCall {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                stream,
            });
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| EngineError::Malformed("no scripted response".into()))
        }
    }

    /// A mock executor that records calls and returns canned results.
    struct MockExecutor {
        results: tokio::sync::Mutex<VecDeque<String>>,
        calls: tokio::sync::Mutex<Vec<(String, String)>>,
    }

    impl MockExecutor {
        fn new(results: Vec<String>) -> Self {
            Self {
                results: tokio::sync::Mutex::new(results.into()),
                calls: tokio::sync::Mutex::new(Vec::new()),
            }
        }

        async fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().await.clone()
        }
    }

    #[async_trait]
    impl ToolExecutor for MockExecutor {
        async fn execute(&self, name: &str, args_json: &str) -> Result<ToolOutcome, ToolError> {
            self.calls
                .lock()
                .await
                .push((name.to_string(), args_json.to_string()));
            let text = self
                .results
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| ToolError::Execute {
                    name: name.to_string(),
                    message: "no scripted result".into(),
                })?;
            Ok(ToolOutcome { text })
        }
    }

    fn comp(content: &str, tool_calls: Vec<ToolCall>) -> Completion {
        let finish = if tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        Completion {
            content: content.into(),
            reasoning_content: None,
            tool_calls,
            finish_reason: Some(finish.into()),
            metrics: Metrics {
                ttft_ms: None,
                total_ms: 1.0,
                tokens_per_sec: None,
                prompt_tokens: None,
                cached_tokens: None,
                completion_tokens: None,
                reasoning_tokens: None,
            },
        }
    }

    fn tc(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            index: 0,
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    fn tool_schema(name: &str) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    /// A recording engine that stamps a wall-clock [`std::time::Instant`] per
    /// `chat` call (in addition to recording calls) so a test can order
    /// external events (e.g. an older lease release) against the turn calls.
    struct TimedEngine {
        responses: tokio::sync::Mutex<VecDeque<Completion>>,
        call_times: tokio::sync::Mutex<Vec<std::time::Instant>>,
    }

    /// A completion carrying the low cache-hit-ratio metrics that trip the
    /// yield gate: `cached = 0` over a large `prompt`.
    fn timed_comp(content: &str, tool_calls: Vec<ToolCall>) -> Completion {
        let finish = if tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        Completion {
            content: content.into(),
            reasoning_content: None,
            tool_calls,
            finish_reason: Some(finish.into()),
            metrics: Metrics {
                ttft_ms: None,
                total_ms: 1.0,
                tokens_per_sec: None,
                prompt_tokens: Some(20_000),
                cached_tokens: Some(0),
                completion_tokens: None,
                reasoning_tokens: None,
            },
        }
    }

    #[async_trait]
    impl ChatEngine for TimedEngine {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
            _stream: bool,
            _reasoning_effort: Option<&str>,
        ) -> Result<Completion, EngineError> {
            self.call_times.lock().await.push(std::time::Instant::now());
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| EngineError::Malformed("no scripted response".into()))
        }
    }

    static TMP_RUNNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmp_state() -> StateDir {
        let n = TMP_RUNNER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "castor_runner_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            n
        ));
        let state = StateDir::new(&dir);
        let _ = state.ensure();
        state
    }

    fn logger_for(state: &StateDir) -> EventLogger {
        EventLogger::new(state, "test_session")
    }

    #[tokio::test]
    async fn happy_path_tool_then_final() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "{\"cmd\":\"ls\"}")]),
                comp("All done.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec!["file1\nfile2".into()]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "do the thing",
            &[tool_schema("bash")],
            80,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.final_text, "All done.");
        assert_eq!(res.status, "completed");
        assert_eq!(res.turns, 2);
        assert_eq!(res.budget, 80);

        // The tool was executed.
        let calls = executor.calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "bash");

        // The tool result was appended as a tool message (visible in the 2nd call).
        let second_call_msgs = &recorder.calls.lock().await[1].messages;
        assert!(
            second_call_msgs
                .iter()
                .any(|m| m.role == "tool" && m.content == "file1\nfile2")
        );

        // The final event was recorded.
        let events = logger.read_all();
        let final_ev = events
            .iter()
            .find(|e| e["type"] == "final" && e["status"] == "completed")
            .expect("final event recorded");
        // The final event reports the session wall-clock runtime (always
        // present, even if zero for an instant run).
        assert!(
            final_ev
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .is_some(),
            "final event must carry duration_ms: {final_ev}"
        );
    }

    #[tokio::test]
    async fn image_tool_outcome_attaches_visual_block() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "read_file", "{\"path\":\"img.png\"}")]),
                comp("I see a button in the screenshot.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec![
            "[IMAGE_ATTACHMENT:data:image/png;base64,iVBORw0KGgo=:path:img.png]\nSuccessfully loaded image file 'img.png' (12 bytes) for visual inspection.".into(),
        ]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "look at img.png",
            &[tool_schema("read_file")],
            80,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.final_text, "I see a button in the screenshot.");
        assert_eq!(res.turns, 2);

        let second_call_msgs = &recorder.calls.lock().await[1].messages;
        assert!(
            second_call_msgs
                .iter()
                .any(|m| m.role == "tool" && m.content.contains("Successfully loaded image file"))
        );
        assert!(second_call_msgs.iter().any(|m| {
            m.role == "user"
                && m.content
                    .contains("[Visual Inspection Attachment: data:image/png;base64,iVBORw0KGgo=]")
        }));
    }

    #[tokio::test]
    async fn multi_turn_continuation() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "read", "{\"path\":\"a\"}")]),
                comp("", vec![tc("c2", "read", "{\"path\":\"b\"}")]),
                comp("Done.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec!["content-a".into(), "content-b".into()]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "read two files",
            &[tool_schema("read")],
            80,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.final_text, "Done.");
        assert_eq!(res.status, "completed");
        assert_eq!(res.turns, 3);

        // Both tools were executed in order.
        let calls = executor.calls().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read");
        assert_eq!(calls[1].0, "read");
    }

    #[tokio::test]
    async fn budget_exhaustion_landing_notice_and_best_effort_final() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // The model keeps calling tools; the budget (3) is exhausted.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "1")]),
                comp("", vec![tc("c2", "bash", "2")]),
                comp("", vec![tc("c3", "bash", "3")]),
                comp("Best-effort final.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec!["r1".into(), "r2".into(), "r3".into()]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "work",
            &[tool_schema("bash")],
            3,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.status, "completed_budget_exhausted");
        assert_eq!(res.final_text, "Best-effort final.");
        assert_eq!(res.budget, 3);

        // The landing notice was injected into a call's messages.
        let calls = recorder.calls.lock().await;
        let notice_injected = calls.iter().any(|c| {
            c.messages
                .iter()
                .any(|m| m.content.contains("[Budget Notice]"))
        });
        assert!(
            notice_injected,
            "landing notice must be present in messages"
        );

        // The best-effort synthesis call had no tools (stripped).
        let last_call = calls.last().unwrap();
        assert!(
            last_call.tools.is_empty(),
            "final synthesis must strip tools"
        );
    }

    #[tokio::test]
    async fn degenerate_final_triggers_salvage_retry() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // Tool call, then an empty final, then a salvage response.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "x")]),
                comp("   ", Vec::new()),
                comp("Salvaged summary.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec!["ok".into()]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "work",
            &[tool_schema("bash")],
            80,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.final_text, "Salvaged summary.");
        assert_eq!(res.status, "completed");

        // The salvage prompt was injected into a call's messages.
        let calls = recorder.calls.lock().await;
        let salvage_injected = calls
            .iter()
            .any(|c| c.messages.iter().any(|m| m.content.contains("[Salvage]")));
        assert!(
            salvage_injected,
            "salvage prompt must be present in messages"
        );
    }

    #[tokio::test]
    async fn repeated_identical_tool_call_trips_loop_detection() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // The model repeats the identical tool call 4 times.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "same")]),
                comp("", vec![tc("c2", "bash", "same")]),
                comp("", vec![tc("c3", "bash", "same")]),
                comp("", vec![tc("c4", "bash", "same")]),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec!["r".into(), "r".into(), "r".into(), "r".into()]);

        let err = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "work",
            &[tool_schema("bash")],
            80,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, RunnerError::LoopDetected { .. }),
            "expected LoopDetected, got {err:?}"
        );

        // The advisory was injected into a call's messages.
        let calls = recorder.calls.lock().await;
        let advisory_injected = calls.iter().any(|c| {
            c.messages
                .iter()
                .any(|m| m.content.contains("[Loop Advisory]"))
        });
        assert!(
            advisory_injected,
            "loop advisory must be present in messages"
        );
    }

    #[tokio::test]
    async fn tool_error_becomes_a_message_and_loop_continues() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // A tool that errors, then a final.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "boom")]),
                comp("Recovered.", Vec::new()),
            ],
            recorder.clone(),
        );
        // The executor returns an error for the first call.
        // We need the executor to return an error. Use a custom one.
        struct ErrExecutor {
            calls: tokio::sync::Mutex<Vec<(String, String)>>,
        }
        #[async_trait]
        impl ToolExecutor for ErrExecutor {
            async fn execute(&self, name: &str, args_json: &str) -> Result<ToolOutcome, ToolError> {
                self.calls
                    .lock()
                    .await
                    .push((name.to_string(), args_json.to_string()));
                Err(ToolError::Execute {
                    name: name.to_string(),
                    message: "boom failed".into(),
                })
            }
        }
        let err_exec = ErrExecutor {
            calls: tokio::sync::Mutex::new(Vec::new()),
        };

        let res = run_session(
            &engine,
            &err_exec,
            &logger,
            "sys",
            "work",
            &[tool_schema("bash")],
            80,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.final_text, "Recovered.");
        assert_eq!(res.status, "completed");

        // The tool error became a message (visible in the 2nd call).
        let calls = recorder.calls.lock().await;
        let second_call_msgs = &calls[1].messages;
        assert!(
            second_call_msgs
                .iter()
                .any(|m| m.role == "tool" && m.content.contains("Error:"))
        );
    }

    #[tokio::test]
    async fn tool_schemas_are_sent_last() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        let engine = RecordingEngine::new(vec![comp("Done.", Vec::new())], recorder.clone());
        let executor = MockExecutor::new(Vec::new());

        let tools = vec![tool_schema("bash"), tool_schema("read")];
        let res = run_session(&engine, &executor, &logger, "sys", "work", &tools, 80, None)
            .await
            .unwrap();

        assert_eq!(res.final_text, "Done.");
        assert_eq!(res.status, "completed");

        // The tools were sent in the request.
        let calls = recorder.calls.lock().await;
        let first_call_tools = &calls[0].tools;
        assert_eq!(first_call_tools.len(), 2);
        assert_eq!(first_call_tools[0].name, "bash");
        assert_eq!(first_call_tools[1].name, "read");
    }

    /// A completion with a chosen `finish_reason` (used to emit the
    /// reasoning-ceiling sentinel `"length"`).
    fn comp_finish(content: &str, finish: &str) -> Completion {
        Completion {
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            finish_reason: Some(finish.into()),
            metrics: Metrics {
                ttft_ms: None,
                total_ms: 1.0,
                tokens_per_sec: None,
                prompt_tokens: None,
                cached_tokens: None,
                completion_tokens: None,
                reasoning_tokens: None,
            },
        }
    }

    #[tokio::test]
    async fn reasoning_ceiling_on_last_turn_is_not_reclassified_as_cooperative() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // Budget of 1: the single loop turn hits the ceiling (turns == budget
        // after the turn), then the salvage call. Without the honest-status
        // guard, the trailing "landed on the last turn" branch would mask
        // `reasoning_budget_exhausted` as `completed_budget_exhausted`.
        let engine = RecordingEngine::new(
            vec![
                comp_finish("died on the last allowed turn", "length"),
                comp_finish("gap: did not finish", "stop"),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(Vec::new());

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "SYS",
            "THE-PROMPT",
            &[tool_schema("bash")],
            1, // exactly one allowed turn
            Some(&SessionOptions::with_state(state.clone())),
        )
        .await
        .unwrap();

        // The honest status survives the last-turn cooperative-landing branch.
        assert_eq!(
            res.status, "reasoning_budget_exhausted",
            "the reasoning ceiling must not be reclassified as cooperative landing"
        );
        assert_eq!(res.budget, 1);
        assert_eq!(res.turns, 1);

        let _ = std::fs::remove_dir_all(state.root());
    }

    /// A scripted engine that errors on a chosen call index (0-based).
    ///
    /// Used to verify the salvage path logs a failed salvage cleanly without
    /// propagating an error out of `run_session`.
    struct FailAtEngine {
        responses: tokio::sync::Mutex<VecDeque<Completion>>,
        fail_at: usize,
        calls: tokio::sync::Mutex<usize>,
    }

    #[async_trait]
    impl ChatEngine for FailAtEngine {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
            _stream: bool,
            _reasoning_effort: Option<&str>,
        ) -> Result<Completion, EngineError> {
            let n = *self.calls.lock().await;
            if n == self.fail_at {
                return Err(EngineError::Malformed("salvage engine blew up".into()));
            }
            *self.calls.lock().await = n + 1;
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| EngineError::Malformed("no scripted response".into()))
        }
    }

    #[tokio::test]
    async fn reasoning_ceiling_salvage_preserves_history_and_writes_report() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // Two tool turns (building history), then the model hits its reasoning
        // ceiling on a non-tool turn (finish_reason "length"), then the salvage
        // call returns a plain-text report.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "a")]),
                comp("", vec![tc("c2", "read", "b")]),
                comp_finish("partial reasoning, then the ceiling", "length"),
                comp_finish(
                    "Findings: X=42. Table: [row1, row2]. Gap: did not run the tests.",
                    "stop",
                ),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec!["RESULT-A".into(), "RESULT-B".into()]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "SYS",
            "THE-PROMPT",
            &[tool_schema("bash")],
            80,
            Some(&SessionOptions::with_state(state.clone())),
        )
        .await
        .unwrap();

        // (5) Honest status invariant: never masked as success.
        assert_eq!(res.status, "reasoning_budget_exhausted");
        // The salvage path is annotated in final_text.
        assert!(
            res.final_text.contains("reasoning_budget_exhausted"),
            "final_text must annotate the honest status: {}",
            res.final_text
        );

        let calls = recorder.calls.lock().await;
        // calls: 0=turn1(tool), 1=turn2(tool), 2=ceiling turn, 3=salvage.
        let salvage_call = &calls[3];
        // (3) The salvage call has full tools enabled and runs non-streaming.
        assert_eq!(
            salvage_call.tools.len(),
            1,
            "salvage must have full tools enabled"
        );
        assert!(
            !salvage_call.stream,
            "salvage must run non-streaming (low effort)"
        );
        // The normal loop turns ran streaming (contrast).
        assert!(calls[0].stream, "turn 1 must be streaming");

        // (1) Conversation history is preserved: the salvage call sees the full
        //     history — the original user prompt, both tool results, and the
        //     injected non-coercive salvage prompt.
        let salvage_msgs = &salvage_call.messages;
        let has = |needle: &str| salvage_msgs.iter().any(|m| m.content.contains(needle));
        assert!(has("THE-PROMPT"), "original user prompt must be preserved");
        assert!(has("RESULT-A"), "first tool result must be preserved");
        assert!(has("RESULT-B"), "second tool result must be preserved");
        assert!(
            has("[Salvage]"),
            "the non-coercive salvage prompt must be injected"
        );
        // (2) The salvage prompt is peer-empowering: it asks for grounded
        //     summaries, metrics, and blockers without coercive imperatives.
        let salvage_prompt = salvage_msgs
            .iter()
            .find(|m| m.content.contains("[Salvage]"))
            .expect("salvage prompt present");
        let p = salvage_prompt.content.to_lowercase();
        assert!(
            p.contains("summarize"),
            "salvage prompt must ask for a summary: {salvage_prompt:?}"
        );
        assert!(
            !p.contains("land now"),
            "salvage prompt must not carry a coercive 'land now' imperative"
        );

        // (4) The report was written to <state>/.scratch/salvage_<session_id>.md.
        let report = state
            .root()
            .join(".scratch")
            .join("salvage_test_session.md");
        assert!(
            report.exists(),
            "salvage report must be written: {report:?}"
        );
        let body = std::fs::read_to_string(&report).unwrap();
        assert!(body.contains("reasoning_budget_exhausted"));
        assert!(body.contains("Findings: X=42"));
        assert!(body.contains("Gap: did not run the tests"));

        // The ledger recorded a salvage event tagged with the ceiling reason.
        let events = logger.read_all();
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "salvage" && e["reason"] == "reasoning_ceiling"),
            "a salvage event tagged reasoning_ceiling must be in the ledger"
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn reasoning_ceiling_salvage_executes_tool_call_and_synthesizes() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "cargo test")]),
                comp_finish("thinking... reached ceiling", "length"),
                comp("", vec![tc("c2", "read", ".scratch/audit.log")]),
                comp_finish(
                    "Findings: scratchpad log verified 100% pass rate across all suites.",
                    "stop",
                ),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec![
            "CARGO_TEST_OUTPUT".into(),
            "SCRATCHPAD_LOG_CONTENT: 100% pass".into(),
        ]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "SYS",
            "THE-PROMPT",
            &[tool_schema("bash"), tool_schema("read")],
            80,
            Some(&SessionOptions::with_state(state.clone())),
        )
        .await
        .unwrap();

        assert_eq!(res.status, "reasoning_budget_exhausted");
        assert!(
            res.final_text
                .contains("scratchpad log verified 100% pass rate")
        );

        let calls = recorder.calls.lock().await;
        assert_eq!(calls.len(), 4);
        assert_eq!(
            calls[2].tools.len(),
            2,
            "salvage call must receive full tools"
        );
        assert!(
            calls[3].tools.is_empty(),
            "follow-up synthesis must be plain text"
        );

        let events = logger.read_all();
        assert!(
            events.iter().any(|e| e["type"] == "tool_result"
                && e["output"] == "SCRATCHPAD_LOG_CONTENT: 100% pass")
        );
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "salvage" && e["reason"] == "reasoning_ceiling")
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn reasoning_ceiling_salvage_empty_does_not_crash_or_mask() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // Tool turn, then ceiling, then a salvage that returns whitespace.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", "a")]),
                comp_finish("died mid-thought", "length"),
                comp("   ", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec!["ok".into()]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "SYS",
            "THE-PROMPT",
            &[tool_schema("bash")],
            80,
            Some(&SessionOptions::with_state(state.clone())),
        )
        .await
        .unwrap();

        // Honest status is preserved (never flipped to "failed" or success).
        assert_eq!(res.status, "reasoning_budget_exhausted");
        // The in-band note is honest about the empty salvage.
        assert!(
            res.final_text.contains("no content"),
            "final_text must say no salvage content was available: {}",
            res.final_text
        );
        // No report file is written when the salvage is empty.
        let report = state
            .root()
            .join(".scratch")
            .join("salvage_test_session.md");
        assert!(!report.exists(), "no report when the salvage is empty");

        // The ledger still records the (empty) salvage attempt.
        let events = logger.read_all();
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "salvage" && e["reason"] == "reasoning_ceiling")
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn reasoning_ceiling_salvage_error_is_logged_not_propagated() {
        let state = tmp_state();
        let logger = logger_for(&state);
        // Tool turn (call 0), ceiling turn (call 1), salvage errors (call 2).
        let engine = FailAtEngine {
            responses: tokio::sync::Mutex::new(VecDeque::from(vec![
                comp("", vec![tc("c1", "bash", "a")]),
                comp_finish("hit the ceiling", "length"),
            ])),
            fail_at: 2,
            calls: tokio::sync::Mutex::new(0),
        };
        let executor = MockExecutor::new(vec!["ok".into()]);

        // Must return Ok (not Err): a failed salvage is logged, not propagated.
        let res = run_session(
            &engine,
            &executor,
            &logger,
            "SYS",
            "THE-PROMPT",
            &[tool_schema("bash")],
            80,
            Some(&SessionOptions::with_state(state.clone())),
        )
        .await
        .expect("a failed salvage must not propagate as an error");

        assert_eq!(res.status, "reasoning_budget_exhausted");
        assert!(
            res.final_text.contains("salvage"),
            "final_text must annotate that the salvage failed: {}",
            res.final_text
        );
        // The failed salvage is recorded in the ledger with the error.
        let events = logger.read_all();
        assert!(events.iter().any(|e| e["type"] == "salvage"
            && e["reason"] == "reasoning_ceiling"
            && e.get("error").is_some()));

        let _ = std::fs::remove_dir_all(state.root());
    }

    // --- Multi-action loop integration tests --------------------------------

    #[tokio::test]
    async fn alternating_command_cycle_trips_loop_detector() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        let cmd1 = r#"{"command":"git status"}"#;
        let cmd2 = r#"{"command":"git diff"}"#;
        // Alternating cycle: (cmd1, cmd2) repeated 3 times = 6 actions total (trips advisory),
        // then 7th action trips LoopDetected.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", cmd1)]),
                comp("", vec![tc("c2", "bash", cmd2)]),
                comp("", vec![tc("c3", "bash", cmd1)]),
                comp("", vec![tc("c4", "bash", cmd2)]),
                comp("", vec![tc("c5", "bash", cmd1)]),
                comp("", vec![tc("c6", "bash", cmd2)]),
                comp("", vec![tc("c7", "bash", cmd1)]),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec![
            "s1".into(),
            "d1".into(),
            "s2".into(),
            "d2".into(),
            "s3".into(),
            "d3".into(),
            "s4".into(),
        ]);

        let err = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "work",
            &[tool_schema("bash")],
            80,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, RunnerError::LoopDetected { .. }),
            "alternating action cycles must trip LoopDetector: {err:?}"
        );

        let calls = recorder.calls.lock().await;
        let loop_advisory_injected = calls.iter().any(|c| {
            c.messages
                .iter()
                .any(|m| m.content.contains("[Loop Advisory]"))
        });
        assert!(
            loop_advisory_injected,
            "loop advisory must be injected after 3 cycles before hard stop"
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn long_compiler_or_test_sequence_never_trips_false_impasse() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // 14 distinct commands (compilation, diagnostics, testing).
        // Without ProbeTracker, the session completes without any artificial impasse.
        let mut responses = Vec::new();
        let mut outputs = Vec::new();
        for i in 0..14 {
            let cmd = format!(r#"{{"command":"tectonic run --flag {i}"}}"#);
            responses.push(comp("", vec![tc(&format!("c{i}"), "bash", &cmd)]));
            outputs.push(format!("build pass {i}"));
        }
        responses.push(comp("Document built successfully.", Vec::new()));

        let engine = RecordingEngine::new(responses, recorder.clone());
        let executor = MockExecutor::new(outputs);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "compile the document to PDF using tectonic",
            &[tool_schema("bash")],
            80,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.status, "completed");
        assert_eq!(res.final_text, "Document built successfully.");

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn identical_scratch_command_still_trips_loop_detector() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // The same .scratch/ command repeated 4 times: the LoopDetector
        // (threshold 3) fires regardless of scratchpad exemption.
        let scratch_cmd = r#"{"command":"python .scratch/repro.py"}"#;
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", scratch_cmd)]),
                comp("", vec![tc("c2", "bash", scratch_cmd)]),
                comp("", vec![tc("c3", "bash", scratch_cmd)]),
                comp("", vec![tc("c4", "bash", scratch_cmd)]),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec![
            "python output 1".into(),
            "python output 2".into(),
            "python output 3".into(),
            "python output 4".into(),
        ]);

        let err = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "work",
            &[tool_schema("bash")],
            80,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, RunnerError::LoopDetected { .. }),
            "identical .scratch/ commands must still trip the exact-match LoopDetector: {err:?}"
        );

        // The loop advisory was injected before the hard stop.
        let calls = recorder.calls.lock().await;
        let loop_advisory_injected = calls.iter().any(|c| {
            c.messages
                .iter()
                .any(|m| m.content.contains("[Loop Advisory]"))
        });
        assert!(
            loop_advisory_injected,
            "loop advisory must be present before the hard stop"
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[test]
    fn tool_output_spill_below_threshold_returns_unchanged() {
        let small = "a".repeat(100);
        let (output, spilled) = spill_tool_output(&small, "c1", None, None);
        assert_eq!(output, small);
        assert!(spilled.is_none());
    }

    #[test]
    fn tool_output_spill_above_threshold_spills_to_scratch() {
        let state = tmp_state();
        let large = "x".repeat(SPILL_THRESHOLD_BYTES + 5000);
        let (pointer, spilled) = spill_tool_output(&large, "call_99", Some(state.root()), None);
        assert!(spilled.is_some());
        let (bytes, path) = spilled.unwrap();
        assert_eq!(bytes, large.len());
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), large);
        assert!(pointer.contains("Tool output spilled to disk"));
        assert!(pointer.contains("--- Head preview"));
        assert!(pointer.contains("--- Tail preview"));
        let _ = std::fs::remove_dir_all(state.root());
    }

    // --- Issue #15: workspace skeleton & custom spill threshold -----------

    #[test]
    fn build_workspace_skeleton_is_deterministic_sorted_and_excludes_build_state() {
        let dir = std::env::temp_dir().join(format!(
            "castor_skel_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Deliberately create entries in a non-sorted order, including the
        // ignored build-state dirs.
        for name in [
            "zzz.txt",
            "src",
            "target",
            "node_modules",
            ".git",
            ".scratch",
            "Makefile",
            "Cargo.toml",
            "aaa.txt",
        ] {
            let p = dir.join(name);
            if name == "src" {
                std::fs::create_dir_all(&p).unwrap();
            } else {
                std::fs::write(&p, "x").unwrap();
            }
        }

        let a = build_workspace_skeleton(&dir);
        let b = build_workspace_skeleton(&dir);
        // Deterministic: two calls over the same workspace yield identical bytes.
        assert_eq!(a, b);
        // Header present.
        assert!(a.starts_with("[Workspace Skeleton]\n"));
        // Ignored build-state dirs are excluded.
        for ignored in [".git", "target", "node_modules", ".scratch"] {
            assert!(
                !a.contains(&format!("- {ignored}\n")),
                "skeleton must not list ignored entry `{ignored}`:\n{a}"
            );
        }
        // Present non-hidden source dirs are listed with a trailing `/` and in
        // sorted lexicographical order; canonical manifests are listed after
        // them (also sorted).
        let lines: Vec<String> = a
            .lines()
            .skip(1) // drop the header
            .filter(|l| l.starts_with("- "))
            .map(str::to_owned)
            .collect();
        // The exact expected: source dirs first (with trailing `/`, sorted),
        // then the canonical manifest (sorted).
        let mut dirs_expected = ["src".to_string()];
        dirs_expected.sort();
        let mut files_expected = ["Cargo.toml".to_string()];
        files_expected.sort();
        let expected: Vec<String> = dirs_expected
            .iter()
            .map(|n| format!("- {n}/"))
            .chain(files_expected.iter().map(|n| format!("- {n}")))
            .collect();
        assert_eq!(
            lines, expected,
            "entries must be dirs (trailing `/`) then manifests, each sorted:\n{a}"
        );

        // Finding 4: creating an arbitrary untracked file in the workspace
        // root must NOT change the skeleton — it is not a canonical manifest,
        // so the prefix stays 100% byte-identical.
        let before = build_workspace_skeleton(&dir);
        std::fs::write(dir.join("random_output.mp4"), "binary").unwrap();
        std::fs::write(dir.join("scratch_note.txt"), "note").unwrap();
        let after = build_workspace_skeleton(&dir);
        assert_eq!(
            before, after,
            "skeleton must be byte-identical when arbitrary untracked files are added:\n{before}\n--\n{after}"
        );
        assert!(
            !after.contains("random_output.mp4") && !after.contains("scratch_note.txt"),
            "arbitrary untracked top-level files must never leak into the skeleton:\n{after}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spill_tool_output_respects_custom_threshold() {
        let state = tmp_state();
        // A payload that exceeds a *custom* low threshold but is well below the
        // default 16 KiB — proves the caller's `threshold` overrides the default.
        let custom = 100usize;
        let payload = "y".repeat(300);
        assert!(payload.len() > custom && payload.len() < SPILL_THRESHOLD_BYTES);

        let (pointer, spilled) =
            spill_tool_output(&payload, "custom_1", Some(state.root()), Some(custom));
        assert!(
            spilled.is_some(),
            "a custom threshold below the payload length must trigger a spill"
        );
        let (bytes, path) = spilled.unwrap();
        assert_eq!(bytes, payload.len());
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), payload);
        assert!(pointer.contains("Tool output spilled to disk"));
        assert!(pointer.contains(&format!("> {custom}-byte threshold")));

        // Same payload, but a threshold above its length must NOT spill.
        let (output, spilled2) = spill_tool_output(
            &payload,
            "custom_2",
            Some(state.root()),
            Some(payload.len()),
        );
        assert_eq!(output, payload);
        assert!(spilled2.is_none());

        let _ = std::fs::remove_dir_all(state.root());
    }

    // --- Session-history persistence & 75% watermark ----------------------

    #[test]
    fn save_and_load_round_trips_sanitized_non_system_history() {
        let state = tmp_state();
        let messages = vec![
            msg("system", "SYS"),
            msg("user", "do the thing"),
            Message {
                role: "assistant".into(),
                content: "\u{3c}think\u{3e} reasoning 42 \u{3c}/think\u{3e} the answer is 42"
                    .into(),
                tool_calls: vec![tc("c1", "bash", r#"{"cmd":"ls"}"#)],
                tool_call_id: None,
            },
            Message {
                role: "tool".into(),
                content: "file1".into(),
                tool_calls: Vec::new(),
                tool_call_id: Some("c1".into()),
            },
        ];
        save_messages(&state, "roundtrip", &messages);

        // The file lands at <state>/sessions/<id>/messages.json.
        let path = state.sessions().join("roundtrip").join("messages.json");
        assert!(path.exists(), "messages.json must be written: {path:?}");

        let loaded = load_messages(&state, "roundtrip").expect("history must load");

        // System message is dropped; 3 non-system messages remain.
        assert_eq!(loaded.len(), 3, "system must be dropped; got {loaded:?}");
        assert!(
            !loaded.iter().any(|m| m.role == "system"),
            "system must not be persisted"
        );

        // The opening think tag is stripped from the assistant message; the
        // reasoning/answer content survives. (The projection also carries no
        // `reasoning_content` payload at all — the "cleared to None" guarantee.)
        let assistant = loaded.iter().find(|m| m.role == "assistant").unwrap();
        assert!(
            !assistant.content.contains("\u{3c}think\u{3e}"),
            "opening think tag must be stripped: {}",
            assistant.content
        );
        assert!(
            assistant.content.contains("42"),
            "non-think content must survive: {}",
            assistant.content
        );
        // The tool_call and its id survive the round-trip.
        assert_eq!(assistant.tool_calls.len(), 1);
        assert_eq!(assistant.tool_calls[0].name, "bash");
        assert_eq!(assistant.tool_calls[0].id, "c1");

        // The tool message keeps its tool_call_id and content.
        let tool = loaded.iter().find(|m| m.role == "tool").unwrap();
        assert_eq!(tool.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(tool.content, "file1");

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[test]
    fn load_messages_absent_file_is_none() {
        let state = tmp_state();
        // No messages.json was written for this session id.
        let loaded = load_messages(&state, "never_saved");
        assert!(loaded.is_none(), "absent history must load as None");
        let _ = std::fs::remove_dir_all(state.root());
    }

    #[test]
    fn watermark_within_budget_resumes() {
        // max_context = 1000 → 75% watermark = 750 tokens.
        // history: 1 message of 400 chars = 100 tokens; new prompt 100 chars = 25.
        // 100 + 25 = 125 <= 750 → resume.
        let history = vec![msg("user", "a".repeat(400))];
        let mut opts = SessionOptions::with_state(tmp_state());
        opts.max_context = 1000;
        assert!(
            should_resume(&history, &"b".repeat(100), Some(&opts)),
            "history within the 75% watermark must resume"
        );
    }

    #[test]
    fn watermark_over_budget_rolls_over() {
        // max_context = 100 → 75% watermark = 75 tokens.
        // history: 400 chars = 100 tokens; new prompt 100 chars = 25 → 125 > 75.
        let history = vec![msg("user", "a".repeat(400))];
        let mut opts = SessionOptions::with_state(tmp_state());
        opts.max_context = 100;
        assert!(
            !should_resume(&history, &"b".repeat(100), Some(&opts)),
            "history over the 75% watermark must roll over"
        );
    }

    #[test]
    fn watermark_disabled_when_max_context_zero() {
        // max_context = 0 is the default (disabled): any history resumes.
        let history = vec![msg("user", "a".repeat(1_000_000))];
        let opts = SessionOptions::with_state(tmp_state());
        assert_eq!(opts.max_context, 0, "default must be disabled (0)");
        assert!(
            should_resume(&history, "prompt", Some(&opts)),
            "zero max_context disables the watermark → always resume"
        );
    }

    #[test]
    fn watermark_disabled_when_no_options() {
        // No options → no budget known → the watermark is disabled.
        let history = vec![msg("user", "a".repeat(1_000_000))];
        assert!(
            should_resume(&history, "prompt", None),
            "absent options disable the watermark → always resume"
        );
    }

    #[tokio::test]
    async fn second_dispatch_resumes_prior_history() {
        let state = tmp_state();
        let sid = "resume_session";

        // First dispatch: one tool turn + a final. Persists history at the end.
        let logger1 = EventLogger::new(&state, sid);
        let engine1 = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", r#"{"cmd":"ls"}"#)]),
                comp("First done.", Vec::new()),
            ],
            std::sync::Arc::new(CallRecorder::default()),
        );
        let executor1 = MockExecutor::new(vec!["file1".into()]);
        let opts1 = SessionOptions::with_state(state.clone()); // max_context = 0 → resume
        let res1 = run_session(
            &engine1,
            &executor1,
            &logger1,
            "SYS",
            "first prompt",
            &[tool_schema("bash")],
            80,
            Some(&opts1),
        )
        .await
        .unwrap();
        assert_eq!(res1.status, "completed");

        // The persisted file was written at <state>/sessions/<id>/messages.json.
        let path = state.sessions().join(sid).join("messages.json");
        assert!(path.exists(), "messages.json must be persisted: {path:?}");

        // Second dispatch: the first engine call must carry the prior history.
        let logger2 = EventLogger::new(&state, sid);
        let recorder2 = std::sync::Arc::new(CallRecorder::default());
        let engine2 =
            RecordingEngine::new(vec![comp("Second done.", Vec::new())], recorder2.clone());
        let executor2 = MockExecutor::new(Vec::new());
        let opts2 = SessionOptions::with_state(state.clone());
        let res2 = run_session(
            &engine2,
            &executor2,
            &logger2,
            "SYS",
            "second prompt",
            &[tool_schema("bash")],
            80,
            Some(&opts2),
        )
        .await
        .unwrap();
        assert_eq!(res2.status, "completed");

        // The first engine call of the second dispatch must include the prior
        // user prompt, the prior tool result, and the new prompt appended last.
        let first_call = &recorder2.calls.lock().await[0].messages;
        assert!(
            first_call
                .iter()
                .any(|m| m.role == "user" && m.content == "first prompt"),
            "prior user prompt must be resumed"
        );
        assert!(
            first_call
                .iter()
                .any(|m| m.role == "tool" && m.content == "file1"),
            "prior tool result must be resumed"
        );
        let last = first_call.last().unwrap();
        assert_eq!(
            (last.role.as_str(), last.content.as_str()),
            ("user", "second prompt"),
            "the new prompt must be appended last: {first_call:?}"
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn second_dispatch_rolls_over_when_over_watermark() {
        let state = tmp_state();
        let sid = "rollover_session";

        // First dispatch builds a large history (a 4000-char tool output).
        let logger1 = EventLogger::new(&state, sid);
        let big_out = "x".repeat(4000); // ~1000 tokens
        let engine1 = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", r#"{"cmd":"big"}"#)]),
                comp("First done.", Vec::new()),
            ],
            std::sync::Arc::new(CallRecorder::default()),
        );
        let executor1 = MockExecutor::new(vec![big_out]);
        let opts1 = SessionOptions::with_state(state.clone());
        let _ = run_session(
            &engine1,
            &executor1,
            &logger1,
            "SYS",
            "first",
            &[tool_schema("bash")],
            80,
            Some(&opts1),
        )
        .await
        .unwrap();

        // Second dispatch with a tiny max_context → history exceeds 75% →
        // roll over to a fresh [system, user] (no prior history resumed).
        let logger2 = EventLogger::new(&state, sid);
        let recorder2 = std::sync::Arc::new(CallRecorder::default());
        let engine2 =
            RecordingEngine::new(vec![comp("Second done.", Vec::new())], recorder2.clone());
        let executor2 = MockExecutor::new(Vec::new());
        let mut opts2 = SessionOptions::with_state(state.clone());
        opts2.max_context = 100; // 75% = 75 tokens; prior history ~1000+ tokens

        let res2 = run_session(
            &engine2,
            &executor2,
            &logger2,
            "SYS",
            "second",
            &[tool_schema("bash")],
            80,
            Some(&opts2),
        )
        .await
        .unwrap();
        assert_eq!(res2.status, "completed");

        // Rollover: the first engine call must be exactly [system, user].
        let first_call = &recorder2.calls.lock().await[0].messages;
        assert_eq!(
            first_call.len(),
            2,
            "rollover must start fresh [system, user]; got {first_call:?}"
        );
        assert_eq!(first_call[0].role, "system");
        assert_eq!(first_call[1].role, "user");
        assert_eq!(first_call[1].content, "second");

        let _ = std::fs::remove_dir_all(state.root());
    }

    // ── Reasoning-tag hygiene helpers (pure functions) ─────────────────

    #[test]
    fn strip_reasoning_tags_removes_open_and_close_span() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        let content = format!("before {open} reasoning {close} after");
        assert_eq!(strip_reasoning_tags(&content, open, close), "before  after");
    }

    #[test]
    fn strip_reasoning_tags_truncates_dangling_open_tag() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        // An open tag with no matching close → everything from the open tag
        // onward is dropped (the deliberation was severed mid-stream).
        assert_eq!(
            strip_reasoning_tags(&format!("start {open} tail"), open, close),
            "start"
        );
    }

    #[test]
    fn strip_reasoning_tags_is_noop_when_absent() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        assert_eq!(
            strip_reasoning_tags("no tags here", open, close),
            "no tags here"
        );
    }

    #[test]
    fn sanitize_tool_delimiters_inserts_zwsp_after_open_angle() {
        let open = concat!("<th", "ink>");
        let close = concat!("</th", "ink>");
        let text = format!("here {open} body {close} done");
        let out = sanitize_tool_delimiters(&text, open, close);
        // A zero-width space must now sit after the '<' of every tag so the
        // byte sequence no longer reads as a valid XML-like delimiter.
        assert!(out.contains('\u{200b}'), "expected ZWSP: {out:?}");
        assert!(!out.contains(open), "raw open tag survived: {out:?}");
        assert!(!out.contains(close), "raw close tag survived: {out:?}");
        // One ZWSP per tag (two tags) → exactly two extra scalar positions.
        assert_eq!(out.chars().count(), text.chars().count() + 2);
    }

    #[test]
    fn is_severed_stop_true_when_trailing_text_after_close() {
        let close = concat!("</th", "ink>");
        // Deliberation closed but a non-empty answer follows the close tag.
        assert!(is_severed_stop(
            &format!("reason {close} trailing answer"),
            close
        ));
    }

    #[test]
    fn is_severed_stop_false_when_close_at_end_or_absent() {
        let close = concat!("</th", "ink>");
        // Close tag with nothing after it → a clean stop, not a severed one.
        assert!(!is_severed_stop(&format!("reason {close}"), close));
        // No close tag at all → no closed reasoning block to sever on.
        assert!(!is_severed_stop("no close tag", close));
    }

    /// An engine that records the `reasoning_effort` passed to each `chat`
    /// call so tests can verify the watchdog retried at `"low"` (the existing
    /// `RecordingEngine` ignores the effort).
    struct EffortRecordingEngine {
        responses: tokio::sync::Mutex<VecDeque<Completion>>,
        efforts: tokio::sync::Mutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl ChatEngine for EffortRecordingEngine {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
            _stream: bool,
            effort: Option<&str>,
        ) -> Result<Completion, EngineError> {
            self.efforts.lock().await.push(effort.map(str::to_string));
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| EngineError::Malformed("no scripted response".into()))
        }
    }

    fn stall_comp(reasoning_tokens: u64) -> Completion {
        Completion {
            content: String::new(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            finish_reason: Some("length".into()),
            metrics: Metrics {
                ttft_ms: None,
                total_ms: 1.0,
                tokens_per_sec: None,
                prompt_tokens: None,
                cached_tokens: None,
                completion_tokens: None,
                reasoning_tokens: Some(reasoning_tokens),
            },
        }
    }

    /// A normal (non-stalling) completion.
    fn ok_comp() -> Completion {
        Completion {
            content: "done".into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            metrics: Metrics::default(),
        }
    }

    /// No token-budget trigger when the budget is disabled (`0`): the primary
    /// `length`/empty-tool-call completion is returned untouched and the engine
    /// is called exactly once.
    #[tokio::test]
    async fn watchdog_disabled_returns_first_completion_untouched() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let engine = EffortRecordingEngine {
            responses: tokio::sync::Mutex::new(vec![stall_comp(999_999), ok_comp()].into()),
            efforts: tokio::sync::Mutex::new(Vec::new()),
        };
        // `turn_timeout_secs = 0` disables the timeout; `max = 0` disables the
        // token-budget trigger.
        let res = chat_with_watchdog(&engine, &[], &[], None, 0, 0, &logger, 1)
            .await
            .unwrap();
        // The primary (stalling) completion was returned as-is; the retry was
        // never issued.
        assert_eq!(res.finish_reason.as_deref(), Some("length"));
        let efforts = engine.efforts.lock().await;
        assert_eq!(
            *efforts,
            Vec::from([None]),
            "exactly one call, no 'low' retry"
        );
    }

    /// Token-budget trigger: a `length`/empty-tool-call turn that burned at
    /// least the budget is retried once at `"low"` effort, and the retry's
    /// completion is returned.
    #[tokio::test]
    async fn watchdog_reasoning_budget_retries_at_low_effort() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let engine = EffortRecordingEngine {
            responses: tokio::sync::Mutex::new(vec![stall_comp(20_000), ok_comp()].into()),
            efforts: tokio::sync::Mutex::new(Vec::new()),
        };
        let res = chat_with_watchdog(&engine, &[], &[], Some("high"), 0, 16_384, &logger, 1)
            .await
            .unwrap();
        // The retry's (non-stalling) completion is what comes back.
        assert_eq!(res.finish_reason.as_deref(), Some("stop"));
        let efforts = engine.efforts.lock().await;
        assert_eq!(
            *efforts,
            Vec::from([Some("high".into()), Some("low".into())])
        );
        // The stall was logged as a `turn_stall_retry` event.
        let events = logger.read_all();
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "turn_stall_retry" && e["reason"] == "reasoning_token_budget"),
            "expected a reasoning_token_budget stall event: {events:?}"
        );
    }

    /// Below the token budget the turn is NOT treated as a stall: the primary
    /// `length`/empty-tool-call completion is returned and the engine is called
    /// exactly once.
    #[tokio::test]
    async fn watchdog_below_budget_does_not_retry() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let engine = EffortRecordingEngine {
            responses: tokio::sync::Mutex::new(vec![stall_comp(100)].into()),
            efforts: tokio::sync::Mutex::new(Vec::new()),
        };
        let res = chat_with_watchdog(&engine, &[], &[], Some("high"), 0, 16_384, &logger, 1)
            .await
            .unwrap();
        assert_eq!(res.finish_reason.as_deref(), Some("length"));
        let efforts = engine.efforts.lock().await;
        assert_eq!(
            *efforts,
            Vec::from([Some("high".into())]),
            "no retry below budget"
        );
    }

    /// The wall-clock timeout trigger: when the primary chat overruns the
    /// configured ceiling, the watchdog retries once at `"low"` effort.
    #[tokio::test]
    async fn watchdog_timeout_retries_at_low_effort() {
        let state = tmp_state();
        let logger = logger_for(&state);
        // First call hangs longer than the 1s ceiling; the retry returns
        // promptly.
        struct SlowThenOkEngine {
            inner: EffortRecordingEngine,
        }
        #[async_trait]
        impl ChatEngine for SlowThenOkEngine {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolSchema],
                _stream: bool,
                effort: Option<&str>,
            ) -> Result<Completion, EngineError> {
                // Record this call up-front (before any await) so a dropped
                // primary still leaves a marker; the retry then sees count 1 and
                // does not sleep.
                let first = self.inner.efforts.lock().await.is_empty();
                self.inner
                    .efforts
                    .lock()
                    .await
                    .push(effort.map(str::to_string));
                if first {
                    // Primary call: overruns the 1s ceiling.
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                self.inner
                    .responses
                    .lock()
                    .await
                    .pop_front()
                    .ok_or_else(|| EngineError::Malformed("no scripted response".into()))
            }
        }
        let inner = EffortRecordingEngine {
            responses: tokio::sync::Mutex::new(vec![ok_comp()].into()),
            efforts: tokio::sync::Mutex::new(Vec::new()),
        };
        let engine = SlowThenOkEngine { inner };
        let res = chat_with_watchdog(&engine, &[], &[], Some("high"), 1, 0, &logger, 1)
            .await
            .unwrap();
        assert_eq!(res.finish_reason.as_deref(), Some("stop"));
        // Only the retry recorded an effort (the timed-out primary was dropped
        // before it recorded).
        let events = logger.read_all();
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "turn_stall_retry" && e["reason"] == "timeout"),
            "expected a timeout stall event: {events:?}"
        );
    }

    /// Finding 5 — true cooperative yield handoff, end-to-end.
    ///
    /// An *older* worker (spawned task) grabs a slot and releases it ~80ms
    /// later. The session runs as a *younger* worker (a 2nd slot, single
    /// tenant) with a low cache-hit ratio. On Turn 1 the yield gate fires;
    /// the runner must release its own slot, **wait until the older lease is
    /// actually freed**, and only then re-acquire and run Turn 2. We prove the
    /// wait by asserting the older release instant lands strictly between the
    /// Turn-1 and Turn-2 engine calls.
    #[tokio::test]
    async fn cooperative_yield_waits_for_older_lease_before_next_turn() {
        use crate::task::semaphore::TaskSemaphore;

        let state = tmp_state();
        let state_root = state.root().to_path_buf();
        let logger = logger_for(&state);
        // Two slots so the older and younger worker can hold one each; the
        // short poll interval keeps the acquire wait tight.
        let sem = TaskSemaphore::with_poll_interval(&state, 2, 5);

        // The older worker: claim a slot, hold it 80ms, then release it.
        let older_release_at: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let rel = older_release_at.clone();
        let sem_old = sem.clone();
        let older = tokio::spawn(async move {
            let lease = sem_old.acquire_with_tokens("task_old", 0).await;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let _ = sem_old.release(&lease);
            *rel.lock().unwrap() = Some(std::time::Instant::now());
        });

        // Give the older worker a clear head start so its `at` is strictly
        // earlier (right-of-way ordering by acquisition timestamp).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The younger worker (this session) claims the 2nd slot.
        let younger = sem.acquire_with_tokens("task_young", 0).await;
        let ctx = LeaseYieldContext {
            sem: std::sync::Arc::new(sem.clone()),
            lease: std::sync::Arc::new(std::sync::Mutex::new(younger)),
            task_id: "task_young".into(),
            min_eviction_detect_tokens: 100,
            cache_hit_yield_ratio: 0.20,
        };
        let options = SessionOptions::with_state(state).with_lease_context(ctx);

        // A recording engine that stamps an `Instant` per `chat` call, so we
        // can order the older release against the two turn calls.
        let engine = TimedEngine {
            responses: tokio::sync::Mutex::new(
                vec![
                    timed_comp("", vec![tc("c1", "bash", "{\"cmd\":\"ls\"}")]),
                    timed_comp("All done.", Vec::new()),
                ]
                .into(),
            ),
            call_times: tokio::sync::Mutex::new(Vec::new()),
        };
        let executor = MockExecutor::new(vec!["file1\nfile2".into()]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "do the thing",
            &[tool_schema("bash")],
            3,
            Some(&options),
        )
        .await
        .unwrap();
        let _ = older.await;

        // The session completed its 2 turns (the final landed on turn 2, a
        // non-last turn of the 3-turn budget, so the status is a clean
        // "completed" — not "completed_budget_exhausted").
        assert_eq!(res.status, "completed");
        assert_eq!(res.turns, 2);

        // The yield gate fired exactly once (the younger, low-cache worker on
        // Turn 1) and was logged.
        let events = logger.read_all();
        assert!(
            events.iter().any(|e| e["type"] == "cooperative_yield"),
            "expected a cooperative_yield event: {events:?}"
        );

        // The older release must land strictly between the Turn-1 and Turn-2
        // calls — i.e. the younger worker actually waited on the older lease
        // (it did not just drop and immediately re-claim its own slot).
        let call_times = engine.call_times.lock().await.clone();
        assert_eq!(
            call_times.len(),
            2,
            "expected exactly two engine calls: {call_times:?}"
        );
        let older_release_at = older_release_at.lock().unwrap();
        let Some(older_release_at) = older_release_at.as_ref() else {
            panic!("older worker never released its lease");
        };
        assert!(
            call_times[0] < *older_release_at && *older_release_at <= call_times[1],
            "older release must be waited on between turn 1 ({:?}) and turn 2 ({:?})",
            call_times[0],
            call_times[1]
        );
        let _ = std::fs::remove_dir_all(&state_root);
    }
}
