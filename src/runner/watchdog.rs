//! Wall-clock timeout watchdog and token monitoring for chat dispatches.

use tracing::warn;

use crate::engine::{Completion, EngineError, Message, ToolSchema};
use crate::runner::ChatEngine;
use crate::runner::events::EventLogger;
use crate::runner::messages::msg;

/// Chat with watchdog: wraps `engine.chat` in a wall-clock timeout and
/// monitors reasoning token budget ceilings.
#[allow(clippy::too_many_arguments)] // each argument is a distinct watchdog input
pub(crate) async fn chat_with_watchdog(
    engine: &dyn ChatEngine,
    messages: &[Message],
    tools: &[ToolSchema],
    effort: Option<&str>,
    turn_timeout_secs: u64,
    max_turn_reasoning_tokens: u32,
    logger: &EventLogger,
    turn: u32,
) -> Result<Completion, EngineError> {
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
                    "retry_effort": effort,
                }));
                warn!(
                    turn = turn,
                    "turn stalled on wall-clock timeout ({}s); retrying once (prefix preserved, effort = {effort:?})",
                    turn_timeout_secs
                );
                // Zero-invalidation retry: re-send the *same* message prefix
                // with the *same* effort, appending the stall nudge as the
                // final message (after the shared prefix) so the KV prefix
                // cache stays warm. Mutating the effort here would rewrite
                // `chat_template_kwargs` and invalidate the system-prompt
                // prefix from token 0.
                let mut retry_messages = messages.to_vec();
                retry_messages.push(msg(
                    "user",
                    "[System: Previous turn timed out. Do not deliberate; emit your next tool call or final response immediately.]",
                ));
                return engine.chat(&retry_messages, tools, true, effort).await;
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
            "retry_effort": effort,
        }));
        warn!(
            turn = turn,
            reasoning_tokens = completion.metrics.reasoning_tokens,
            "turn stalled at reasoning-token budget ({}); retrying once (prefix preserved, effort = {effort:?})",
            max_turn_reasoning_tokens
        );
        // The stalling completion never emitted an assistant message (no tool
        // calls and empty content), so its deliberation is not in `messages`.
        // Re-ask with an explicit nudge appended at the tail (after the shared
        // prefix) so the retry model knows *why* it is being re-prompted and
        // lands concisely on the next action — without disturbing any cached
        // prompt bytes.
        let mut retry_messages = messages.to_vec();
        retry_messages.push(msg(
            "user",
            "[System: Previous deliberation exceeded the single-turn reasoning \
             budget. Be concise and emit your next tool call or final response \
             immediately.]",
        ));
        return engine.chat(&retry_messages, tools, true, effort).await;
    }

    Ok(completion)
}
