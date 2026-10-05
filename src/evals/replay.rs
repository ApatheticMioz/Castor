//! Trace replayer: a [`ChatEngine`] that replays a recorded session trace.
//!
//! The engine returns the next recorded model response on each `chat` call,
//! in order. When a non-empty trace is exhausted (e.g. the runner's salvage
//! retry or budget-synthesis makes an extra `chat` call beyond the recorded
//! responses), the engine returns a default final so the session can land
//! cleanly. A truly empty trace yields [`EngineError::TraceExhausted`] on
//! the first call.

use std::collections::VecDeque;

use async_trait::async_trait;

use crate::engine::{Completion, EngineError, Message, Metrics, ToolCall, ToolSchema};
use crate::evals::fixture::TraceStep;
use crate::runner::ChatEngine;

/// A [`ChatEngine`] that replays a recorded trace.
///
/// Only the model's responses (`AssistantMessage` steps) are replayed;
/// harness/tool events (`SessionStart`, `ToolCall`, `ToolResult`,
/// `ToolOutputSpilled`, `SessionEnd`) are skipped, since the runner
/// regenerates tool activity by executing the tool calls it receives.
pub struct ReplayEngine {
    responses: std::sync::Mutex<VecDeque<Completion>>,
    /// Whether the trace had at least one recorded response. When a
    /// non-empty trace is exhausted (e.g. the runner's salvage retry or
    /// budget-synthesis makes an extra `chat` call beyond the recorded
    /// responses), the engine returns a default final instead of
    /// `TraceExhausted`, so the session can land cleanly. A truly empty
    /// trace still yields `TraceExhausted` on the first call.
    non_empty: bool,
}

impl ReplayEngine {
    /// Build a replay engine from a recorded trace.
    ///
    /// Each `AssistantMessage` step is converted to a [`Completion`]. The
    /// tool calls' arguments are taken from the authoritative `ToolCall`
    /// events (matched by `tool_call_id`) when present, falling back to the
    /// `AssistantMessage`'s own `toolCalls` references. The `finish_reason`
    /// is `"tool_calls"` when the response carries tool calls, otherwise
    /// `"stop"`.
    pub fn new(steps: Vec<TraceStep>) -> Self {
        // Build a map from tool_call_id → the authoritative args (as a JSON
        // string) from the `ToolCall` events.
        let mut call_args: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for step in &steps {
            if let TraceStep::ToolCall {
                tool_call_id,
                args,
                ..
            } = step
            {
                call_args.insert(tool_call_id.clone(), args.to_string());
            }
        }

        let responses: VecDeque<Completion> = steps
            .into_iter()
            .filter_map(|step| match step {
                TraceStep::AssistantMessage {
                    content,
                    tool_calls,
                    ..
                } => {
                    let tool_calls: Vec<ToolCall> = tool_calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, ref_)| {
                            // Prefer the authoritative `ToolCall` event args
                            // (the `AssistantMessage` may carry a placeholder
                            // in its `toolCalls` references).
                            let arguments = call_args
                                .get(&ref_.id)
                                .cloned()
                                .unwrap_or(ref_.function.arguments);
                            ToolCall {
                                index,
                                id: ref_.id,
                                name: ref_.function.name,
                                arguments,
                            }
                        })
                        .collect();
                    let finish_reason = if tool_calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    };
                    Some(Completion {
                        content,
                        tool_calls,
                        finish_reason: Some(finish_reason.into()),
                        metrics: Metrics {
                            ttft_ms: None,
                            total_ms: 0.0,
                            tokens_per_sec: None,
                            prompt_tokens: None,
                            completion_tokens: None,
                            reasoning_tokens: None,
                        },
                    })
                }
                _ => None,
            })
            .collect();
        let non_empty = !responses.is_empty();
        Self {
            responses: std::sync::Mutex::new(responses),
            non_empty,
        }
    }
}

#[async_trait]
impl ChatEngine for ReplayEngine {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSchema],
        _stream: bool,
        _reasoning_effort: Option<&str>,
    ) -> Result<Completion, EngineError> {
        // The critical section is synchronous (no await while locked), so a
        // std Mutex is sufficient and cheaper than a tokio Mutex.
        let popped = self
            .responses
            .lock()
            .expect("replay engine responses mutex poisoned")
            .pop_front();
        match popped {
            Some(completion) => Ok(completion),
            None if self.non_empty => {
                // The recorded trace is exhausted, but it was non-empty: the
                // runner made an extra call (salvage retry or budget
                // synthesis). Return a default final so the session can land
                // cleanly rather than failing with `TraceExhausted`.
                Ok(Completion {
                    content: String::new(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    metrics: Metrics {
                        ttft_ms: None,
                        total_ms: 0.0,
                        tokens_per_sec: None,
                        prompt_tokens: None,
                        completion_tokens: None,
                        reasoning_tokens: None,
                    },
                })
            }
            None => Err(EngineError::TraceExhausted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReplayEngine;
    use crate::engine::EngineError;
    use crate::evals::fixture::{ToolCallFunction, ToolCallRef, TraceStep};
    use crate::runner::ChatEngine;

    fn assistant(content: &str, tool_calls: Vec<ToolCallRef>) -> TraceStep {
        TraceStep::AssistantMessage {
            timestamp: "t".into(),
            session_id: "s".into(),
            content: content.into(),
            tool_calls,
        }
    }

    fn tool_call_ref(id: &str, name: &str, arguments: &str) -> ToolCallRef {
        ToolCallRef {
            id: id.into(),
            r#type: "function".into(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    #[tokio::test]
    async fn replays_recorded_responses_in_order() {
        let steps = vec![
            assistant("one", Vec::new()),
            assistant("two", Vec::new()),
            assistant("three", Vec::new()),
        ];
        let engine = ReplayEngine::new(steps);

        let r1 = engine.chat(&[], &[], true, None).await.unwrap();
        assert_eq!(r1.content, "one");
        let r2 = engine.chat(&[], &[], true, None).await.unwrap();
        assert_eq!(r2.content, "two");
        let r3 = engine.chat(&[], &[], true, None).await.unwrap();
        assert_eq!(r3.content, "three");

        // Fourth call: the (non-empty) trace is exhausted → the engine
        // returns a default final so the session can land cleanly (salvage
        // retry / budget synthesis), rather than `TraceExhausted`.
        let r4 = engine.chat(&[], &[], true, None).await.unwrap();
        assert_eq!(r4.content, "");
        assert!(r4.tool_calls.is_empty());
        assert_eq!(r4.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn empty_trace_errors_on_first_call() {
        let engine = ReplayEngine::new(Vec::new());
        let err = engine.chat(&[], &[], true, None).await.unwrap_err();
        assert!(matches!(err, EngineError::TraceExhausted), "{err:?}");
    }

    /// The `tool_call` event is authoritative: when the `assistant_message`
    /// carries a placeholder in its `toolCalls` reference (the real content
    /// lives in the `tool_call` event's `args`), the replayed completion must
    /// use the `tool_call` event's args.
    #[tokio::test]
    async fn tool_call_event_args_prefer_over_assistant_placeholder() {
        let steps = vec![
            assistant(
                "writing the file",
                vec![tool_call_ref(
                    "call_1",
                    "write_file",
                    "{\"path\":\"src/legacy.js\",\"content\":\"<refactor>\"}",
                )],
            ),
            TraceStep::ToolCall {
                timestamp: "t".into(),
                session_id: "s".into(),
                tool_call_id: "call_1".into(),
                name: "write_file".into(),
                args: serde_json::json!({"path": "src/legacy.js", "content": "REAL"}),
            },
        ];
        let engine = ReplayEngine::new(steps);

        let r = engine.chat(&[], &[], true, None).await.unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "call_1");
        assert_eq!(r.tool_calls[0].name, "write_file");
        // The placeholder from the assistant message must NOT leak through.
        // (serde_json serializes object keys in sorted order.)
        assert_eq!(
            r.tool_calls[0].arguments,
            "{\"content\":\"REAL\",\"path\":\"src/legacy.js\"}"
        );
    }

    #[tokio::test]
    async fn tool_calls_pass_through_unchanged() {
        let steps = vec![assistant(
            "calling a tool",
            vec![
                tool_call_ref("call_1", "bash", "{\"cmd\":\"ls\"}"),
                tool_call_ref("call_2", "read", "{\"path\":\"/tmp\"}"),
            ],
        )];
        let engine = ReplayEngine::new(steps);

        let r = engine.chat(&[], &[], true, None).await.unwrap();
        assert_eq!(r.content, "calling a tool");
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].id, "call_1");
        assert_eq!(r.tool_calls[0].name, "bash");
        assert_eq!(r.tool_calls[0].arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(r.tool_calls[1].id, "call_2");
        assert_eq!(r.tool_calls[1].name, "read");
        assert_eq!(r.tool_calls[1].arguments, "{\"path\":\"/tmp\"}");
    }
}
