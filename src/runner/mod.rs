//! Session runner: the multi-turn agent loop.
//!
//! - [`ToolExecutor`]: the async trait the runner depends on for tool execution.
//! - [`ChatEngine`]: the async trait the runner depends on for engine chat.
//! - [`run_session`]: build messages (tool schemas last, vLLM APC contract) →
//!   engine chat → execute tool calls → append results → continue until the
//!   model produces a final or the turn budget is exhausted.
//!
//! Budgets, cooperative landing, loop detection, and the JSONL event ledger
//! live here; the loop detector and event logger are split into submodules.

pub mod events;
pub mod loop_detector;

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;
use tracing::warn;

use crate::engine::{EngineClient, EngineError, Message, Metrics, ToolSchema};
use crate::state::StateDir;

use events::EventLogger;
use loop_detector::{LoopDetector, LoopState, PROBE_ADVISORY, ProbeTracker};

/// Default turn budget (from the task record; extendable up to `MAX_ELASTIC_TURNS`).
pub const DEFAULT_TURNS_BUDGET: u32 = 80;

/// Turns before the end of the budget at which a landing notice is injected.
const LANDING_WINDOW: u32 = 5;

/// `finish_reason` value the engine reports when a response is truncated at
/// its reasoning/output token ceiling. This is the signal for a
/// `reasoning_budget_exhausted` termination (Issue #3 / R2).
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
/// probe-budget advisory is injected (Issue #17 Part B).
pub const DEFAULT_PROBE_BUDGET: usize = 4;

/// The async trait the runner depends on for tool execution.
///
/// M7 implements real tools behind this trait; the runner depends only on it.
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
    /// optionally carrying a per-session reasoning_effort tier (Issue #3).
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
    #[error("loop detected: repeated identical action '{name}'")]
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

/// The per-turn `metrics` object embedded in a `dispatch` event, in the same
/// camelCase shape the telemetry projection (`parse_session_events`) reads
/// (`promptTokens` / `completionTokens` / `reasoningTokens`).
///
/// Only the engine-reported token counts are emitted, so an absent field is
/// omitted rather than zero-filled (honest telemetry: `None` never masquerades
/// as a measured zero).
fn metrics_json(m: &Metrics) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    if let Some(p) = m.prompt_tokens {
        o.insert("prompt_tokens".into(), serde_json::json!(p));
        o.insert("promptTokens".into(), serde_json::json!(p));
    }
    if let Some(c) = m.completion_tokens {
        o.insert("completion_tokens".into(), serde_json::json!(c));
        o.insert("completionTokens".into(), serde_json::json!(c));
    }
    if let Some(r) = m.reasoning_tokens {
        o.insert("reasoning_tokens".into(), serde_json::json!(r));
        o.insert("reasoningTokens".into(), serde_json::json!(r));
    }
    if let Some(r) = m.tokens_per_sec {
        o.insert("tokensPerSec".into(), serde_json::json!(r));
    }
    if let Some(t) = m.ttft_ms {
        o.insert("ttftMs".into(), serde_json::json!(t));
    }
    o.insert("totalMs".into(), serde_json::json!(m.total_ms));
    serde_json::Value::Object(o)
}

/// State / workspace context the runner uses for terminal artifacts.
///
/// This is deliberately *only* the durable-location context — not the engine,
/// executor, or budget — because those already flow through [`run_session`]
/// directly. It exists so a session can persist a salvage report outside the
/// event ledger at terminal (Issue #3 / R2).
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// The castor state directory. `.scratch/` salvage reports are written here
    /// (falling back to the workspace when unset).
    pub state: Option<StateDir>,
    /// The session's working directory. Used as the fallback `.scratch/`
    /// location when no state dir is available.
    pub workspace: Option<PathBuf>,
    /// Per-session reasoning-effort tier (Issue #3), forwarded to the engine
    /// on every chat turn. `None` means "no override": the client sends
    /// neither `reasoning_effort` nor `chat_template_kwargs` and the
    /// server-side default applies (zero behaviour change).
    pub reasoning_effort: Option<String>,
    /// Consecutive non-mutating bash probes before the probe-budget advisory
    /// is injected (Issue #17 Part B). Defaults to 4.
    pub probe_budget: usize,
}

impl SessionOptions {
    /// Build options from just a state directory (the common worker case).
    pub fn with_state(state: StateDir) -> Self {
        Self {
            state: Some(state),
            workspace: None,
            reasoning_effort: None,
            probe_budget: DEFAULT_PROBE_BUDGET,
        }
    }

    /// Build options from just a workspace path (e.g. an offline replay).
    pub fn with_workspace(workspace: PathBuf) -> Self {
        Self {
            state: None,
            workspace: Some(workspace),
            reasoning_effort: None,
            probe_budget: DEFAULT_PROBE_BUDGET,
        }
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

    let mut messages = vec![msg("system", system_prompt), msg("user", user_prompt)];
    let mut loop_detector = LoopDetector::new(LOOP_WINDOW, LOOP_THRESHOLD);
    let probe_budget = options.map_or(DEFAULT_PROBE_BUDGET, |o| o.probe_budget);
    let mut probe_tracker = ProbeTracker::new(probe_budget);
    let mut turns: u32 = 0;
    let mut final_text = String::new();
    let mut final_produced = false;
    let mut tool_activity = false;
    let mut landing_injected = false;
    let mut status = "completed".to_string();

    // Per-session reasoning-effort override (Issue #3). Applied uniformly to
    // every engine call in the session (main turns, salvage, synthesis) —
    // a mid-session tier change would break the prefix cache, so the tier
    // is session-scoped. `None` leaves the payload byte-identical to the
    // pre-override shape (the server-side default applies).
    let effort = options
        .and_then(|o| o.reasoning_effort.as_deref())
        .filter(|e| !e.is_empty());

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
        let completion = engine.chat(&messages, tools, true, effort).await?;

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

        // Append the assistant message.
        messages.push(Message {
            role: "assistant".into(),
            content: completion.content.clone(),
            tool_calls: completion.tool_calls.clone(),
            tool_call_id: None,
        });

        if completion.tool_calls.is_empty() {
            // Compute the ceiling signal before moving `completion.content`.
            let ceiling_hit = is_reasoning_ceiling(&completion);

            // Final content.
            final_text = completion.content;
            final_produced = true;

            // Deliberation-ceiling salvage pass (Issue #3 / R2): the model
            // stopped at its reasoning ceiling on a non-tool turn. We keep the
            // honest status, never wipe the accumulated conversation history,
            // empower the peer engineer with full tools to inspect scratchpads
            // or verify test outputs, and extract a grounded status report.
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
                                let _ = logger.append(serde_json::json!({
                                    "type": "tool_result",
                                    "turn": turns + 1,
                                    "tool_call_id": tc.id,
                                    "name": tc.name,
                                    "is_error": outcome.text.starts_with("Error:"),
                                    "output": outcome.text,
                                }));
                                messages.push(Message {
                                    role: "tool".into(),
                                    content: outcome.text,
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
                logger
                    .append(serde_json::json!({
                        "type": "salvage",
                        "turn": turns,
                        "content": salvage.content,
                    }))
                    .map_err(RunnerError::Io)?;
                if !salvage.content.trim().is_empty() {
                    final_text = salvage.content;
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

            // Append the tool result as a tool message.
            messages.push(Message {
                role: "tool".into(),
                content: outcome.text.clone(),
                tool_call_id: Some(tc.id.clone()),
                tool_calls: Vec::new(),
            });

            // Record the tool_result event.
            logger
                .append(serde_json::json!({
                    "type": "tool_result",
                    "turn": turns,
                    "tool_call_id": tc.id,
                    "name": tc.name,
                    "is_error": outcome.text.starts_with("Error:"),
                    "output": outcome.text,
                }))
                .map_err(RunnerError::Io)?;

            // Loop detection.
            match loop_detector.record(&tc.name, &tc.arguments) {
                LoopState::Ok => {}
                LoopState::Advisory => {
                    let advisory = if tc.name == "read_file" {
                        "[Read Advisory] You have repeated the exact same read on this coordinate \
multiple times. If you are facing contradictory requirements across files or an architectural \
impasse, state your findings and ask for alignment rather than continuing to re-read."
                            .to_string()
                    } else {
                        format!(
                            "[Loop Advisory] You have repeated the same action '{}' multiple times in a \
                             row. Change your approach or synthesize your findings.",
                            tc.name
                        )
                    };
                    messages.push(msg("user", advisory));
                }
                LoopState::LoopDetected => {
                    return Err(RunnerError::LoopDetected {
                        name: tc.name.clone(),
                    });
                }
            }

            // Probe budget tracking (Issue #17 Part B): consecutive
            // non-mutating bash probes that do not target `.scratch/` are
            // counted; at the budget threshold a one-shot advisory is
            // injected to nudge the model toward code mutations.  Mutating
            // tools reset the counter; `.scratch/` commands are exempt.
            if probe_tracker.record(&tc.name, &tc.arguments) {
                messages.push(msg("user", PROBE_ADVISORY));
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
    logger
        .append(serde_json::json!({
            "type": "final",
            "turn": turns,
            "status": status,
            "final_text": final_text,
        }))
        .map_err(RunnerError::Io)?;

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
            tool_calls,
            finish_reason: Some(finish.into()),
            metrics: Metrics {
                ttft_ms: None,
                total_ms: 1.0,
                tokens_per_sec: None,
                prompt_tokens: None,
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

    fn tmp_state() -> StateDir {
        let dir = std::env::temp_dir().join(format!(
            "castor_runner_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "final" && e["status"] == "completed")
        );
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
            tool_calls: Vec::new(),
            finish_reason: Some(finish.into()),
            metrics: Metrics {
                ttft_ms: None,
                total_ms: 1.0,
                tokens_per_sec: None,
                prompt_tokens: None,
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

    // --- Probe budget integration tests (Issue #17 Part B) -------------------

    #[tokio::test]
    async fn non_scratch_bash_probes_trip_probe_budget_advisory() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // Four distinct non-scratch bash commands: the probe budget (default 4)
        // trips on the fourth.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", r#"{"command":"ls"}"#)]),
                comp("", vec![tc("c2", "bash", r#"{"command":"pwd"}"#)]),
                comp("", vec![tc("c3", "bash", r#"{"command":"cat file.txt"}"#)]),
                comp(
                    "",
                    vec![tc("c4", "bash", r#"{"command":"head -1 other.txt"}"#)],
                ),
                comp("Done.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec![
            "file-list".into(),
            "/workspace".into(),
            "file content".into(),
            "first line".into(),
        ]);

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

        assert_eq!(res.status, "completed");

        // The probe advisory was injected into a subsequent call's messages.
        let calls = recorder.calls.lock().await;
        let probe_advisory_injected = calls.iter().any(|c| {
            c.messages
                .iter()
                .any(|m| m.content.contains("[Probe Advisory]"))
        });
        assert!(
            probe_advisory_injected,
            "probe advisory must be present after 4 consecutive non-scratch bash probes"
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn mutating_tool_resets_probe_count() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // Three non-scratch probes, then a write_file (reset), then one more
        // probe. The probe count goes 1→2→3→0→1, so the advisory never fires.
        let engine = RecordingEngine::new(
            vec![
                comp("", vec![tc("c1", "bash", r#"{"command":"ls"}"#)]),
                comp("", vec![tc("c2", "bash", r#"{"command":"pwd"}"#)]),
                comp("", vec![tc("c3", "bash", r#"{"command":"cat a.txt"}"#)]),
                comp(
                    "",
                    vec![tc(
                        "c4",
                        "write_file",
                        r#"{"path":"out.txt","content":"data"}"#,
                    )],
                ),
                comp("", vec![tc("c5", "bash", r#"{"command":"cat b.txt"}"#)]),
                comp("Done.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec![
            "file-list".into(),
            "/workspace".into(),
            "a content".into(),
            "wrote out.txt".into(),
            "b content".into(),
        ]);

        let res = run_session(
            &engine,
            &executor,
            &logger,
            "sys",
            "work",
            &[tool_schema("bash"), tool_schema("write_file")],
            80,
            None,
        )
        .await
        .unwrap();

        assert_eq!(res.status, "completed");

        // No probe advisory should have been injected.
        let calls = recorder.calls.lock().await;
        let probe_advisory_injected = calls.iter().any(|c| {
            c.messages
                .iter()
                .any(|m| m.content.contains("[Probe Advisory]"))
        });
        assert!(
            !probe_advisory_injected,
            "mutating tool must reset the probe count; no advisory should fire"
        );

        let _ = std::fs::remove_dir_all(state.root());
    }

    #[tokio::test]
    async fn scratchpad_bash_commands_are_exempt_from_probe_budget() {
        let state = tmp_state();
        let logger = logger_for(&state);
        let recorder = std::sync::Arc::new(CallRecorder::default());
        // Four distinct .scratch/ commands: all exempt, no probe advisory.
        let engine = RecordingEngine::new(
            vec![
                comp(
                    "",
                    vec![tc(
                        "c1",
                        "bash",
                        r#"{"command":"python .scratch/repro.py"}"#,
                    )],
                ),
                comp(
                    "",
                    vec![tc("c2", "bash", r#"{"command":"bash .scratch/run.sh"}"#)],
                ),
                comp(
                    "",
                    vec![tc("c3", "bash", r#"{"command":"cat .scratch/output.txt"}"#)],
                ),
                comp("", vec![tc("c4", "bash", r#"{"command":"ls .scratch/"}"#)]),
                comp("Done.", Vec::new()),
            ],
            recorder.clone(),
        );
        let executor = MockExecutor::new(vec![
            "python output".into(),
            "bash output".into(),
            "scratch output".into(),
            "scratch listing".into(),
        ]);

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

        assert_eq!(res.status, "completed");

        // No probe advisory, no loop advisory (all commands are distinct).
        let calls = recorder.calls.lock().await;
        let any_advisory = calls.iter().any(|c| {
            c.messages.iter().any(|m| {
                m.content.contains("[Probe Advisory]") || m.content.contains("[Loop Advisory]")
            })
        });
        assert!(
            !any_advisory,
            ".scratch/ commands must be exempt from both probe and loop advisories"
        );

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
}
