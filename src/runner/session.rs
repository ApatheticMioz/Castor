//! Autonomous multi-turn session loop and terminal outcome resolution.

use crate::engine::{Message, ToolSchema};
use crate::runner::budget::{LOOP_THRESHOLD, LOOP_WINDOW};
use crate::runner::context::{history_tokens, load_messages, save_messages, should_resume};
use crate::runner::events::EventLogger;
use crate::runner::loop_detector::{LoopDetector, LoopState};
use crate::runner::messages::{
    is_severed_stop, metrics_json, msg, sanitize_tool_delimiters, strip_reasoning_tags,
};
use crate::runner::salvage::{
    is_reasoning_ceiling, run_degenerate_final_salvage, run_reasoning_ceiling_salvage,
};
use crate::runner::skeleton::spill_tool_output;
use crate::runner::watchdog::chat_with_watchdog;
use crate::runner::{
    ChatEngine, RunnerError, SessionOptions, SessionResult, ToolExecutor, ToolOutcome,
};

/// Helper to decide whether a resumed history should be kept.
fn check_resume(
    history: &[Message],
    new_user_prompt: &str,
    options: Option<&SessionOptions>,
) -> bool {
    let max_context = options.map(|o| o.max_context).unwrap_or(0);
    should_resume(history, new_user_prompt, max_context)
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
    options: Option<&SessionOptions>,
) -> Result<SessionResult, RunnerError> {
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
        if check_resume(&history, user_prompt, options) {
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
    let mut final_text;
    let mut tool_activity = false;
    let mut status = "completed".to_string();

    // Per-session reasoning-effort override, applied uniformly to every
    // engine call in the session (main turns, salvage, synthesis) — a
    // mid-session tier change would break the prefix cache, so the tier
    // is session-scoped. `None` leaves the payload byte-identical to the
    // no-override shape (the server-side default applies).
    let effort = options
        .and_then(|o| o.reasoning_effort.as_deref())
        .filter(|e| !e.is_empty());

    // Per-turn stall watchdog. `0` disables each trigger;
    // both default to disabled so callers without a watchdog are unchanged.
    let (wd_timeout, wd_max_reasoning) = match options {
        Some(o) => (
            o.turn_timeout_secs.unwrap_or(0),
            o.max_turn_reasoning_tokens.unwrap_or(0),
        ),
        None => (0, 0),
    };

    // Reasoning-delimiter tag pair for the session (reserved defaults when
    // `SessionOptions` does not override them). Hoisted once so the per-turn
    // assistant strip, the main tool loop, and the severed-stop continuation
    // all share the same pair (DRY) instead of each re-deriving it.
    let open_tag = options
        .map(|o| o.effective_open_tag())
        .unwrap_or_else(|| concat!("<th", "ink>").to_string());
    let close_tag = options
        .map(|o| o.effective_close_tag())
        .unwrap_or_else(|| concat!("</th", "ink>").to_string());

    loop {
        turns += 1;

        // Pre-turn backpressure: if the aggregate KV pool is under pressure
        // and another heavier or senior active task has priority, pause asynchronously
        // until that task finishes and frees its KV footprint.
        if let Some(ctx) = options.and_then(|o| o.lease_context.as_ref()) {
            let lease = match ctx.lease.lock() {
                Ok(guard) => Some(guard.clone()),
                Err(_) => None,
            };
            if let Some(l) = lease
                && ctx.sem.should_yield(&l)
            {
                let _ = logger.append(serde_json::json!({
                    "type": "cooperative_yield",
                    "turn": turns,
                    "task_id": ctx.task_id,
                    "reason": "pool_backpressure",
                }));
                ctx.sem.wait_backpressure(&l).await;
            }
        }

        let active_tools: &[ToolSchema] = tools;
        let watchdog = wd_timeout > 0 || wd_max_reasoning > 0;
        let completion = if watchdog {
            let primary = chat_with_watchdog(
                engine,
                &messages,
                active_tools,
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
                    engine.chat(&messages, active_tools, true, effort).await?
                }
                Err(err) => return Err(RunnerError::Engine(err)),
            }
        } else {
            match engine.chat(&messages, active_tools, true, effort).await {
                Ok(c) => c,
                Err(crate::engine::EngineError::Sse(err)) => {
                    tracing::warn!(
                        "Transient SSE stream error on turn {turns}: {err}; retrying once"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    engine.chat(&messages, active_tools, true, effort).await?
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
            strip_reasoning_tags(&completion.content, &open_tag, &close_tag)
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

            // Capture the severed-stop signal (reasoning closed, but
            // the model stopped with no tool call and no real content) before
            // moving `completion`. This triggers a one-time continuation prompt
            // so the engine is given a chance to act after deliberating.
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

            // One-time continuation: the model deliberated (closed its reasoning
            // block) but stopped without a tool call and without a real deliverable.
            // Prompt it once to continue acting; if it then emits tool calls, the
            // loop continues naturally — otherwise we land on the new content.
            if severed_stop {
                let continuation_prompt = concat!(
                    "[System: Continuation] Your last turn closed with ",
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
                            // Sanitize embedded reasoning delimiters so a
                            // downstream tool-call parser never sees a valid
                            // delimiter inside this continuation's tool output
                            // (mirrors the main tool loop, DRY).
                            let sanitized_tool_content =
                                sanitize_tool_delimiters(&tool_content, &open_tag, &close_tag);
                            messages.push(Message {
                                role: "tool".into(),
                                content: sanitized_tool_content,
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
                let outcome = run_reasoning_ceiling_salvage(
                    engine,
                    executor,
                    logger,
                    &mut messages,
                    tools,
                    effort,
                    turns,
                    options,
                    session_id,
                )
                .await;
                final_text = outcome.final_text;
                status = outcome.status;
                break;
            }

            // Degenerate final: empty/whitespace after real tool activity.
            if final_text.trim().is_empty() && tool_activity {
                let outcome =
                    run_degenerate_final_salvage(engine, logger, &mut messages, effort, turns)
                        .await?;
                final_text = outcome.final_text;
                status = outcome.status;
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
            // valid `<th_ink>`-style delimiter inside tool output. (The tag pair
            // is hoisted to function scope; see the pre-loop definition.)
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
                        "[System: Read Advisory] You have repeated the exact same read on this coordinate \
multiple times. If you are facing contradictory requirements across files or an architectural \
impasse, state your findings and ask for alignment rather than continuing to re-read."
                            .to_string()
                    } else {
                        "[System: Loop Advisory] You have repeated the same action sequence multiple times in a \
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

        // Per-turn slot bookkeeping: record the turn's token footprint on the held lease
        // so the aggregate KV pool accurately reflects all active sessions.
        if let Some(ctx) = options.and_then(|o| o.lease_context.as_ref()) {
            let prompt_tokens = completion.metrics.prompt_tokens;
            let completion_tokens = completion.metrics.completion_tokens;
            let turn_tokens = prompt_tokens
                .map(|p| p as u32)
                .unwrap_or_else(|| history_tokens(&messages) as u32)
                .saturating_add(completion_tokens.map(|c| c as u32).unwrap_or(0));
            ctx.sem.update_context_tokens(&ctx.task_id, turn_tokens);
        }
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

    // Normal completion: persist the final sanitized history so a later dispatch
    // resumes from here.
    if let Some(state) = options.and_then(|o| o.state.as_ref()) {
        save_messages(state, session_id, &messages);
    }

    Ok(SessionResult {
        final_text,
        turns,
        status,
    })
}
