use super::context::{load_messages, save_messages};
use super::events::EventLogger;
use super::messages::*;
use super::skeleton::{SPILL_THRESHOLD_BYTES, build_workspace_skeleton, spill_tool_output};
use super::watchdog::*;
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

/// One recorded `chat` call from [`ScriptedEngine`]: the wall-clock instant,
/// the exact messages it was invoked with, and the tool list (so tests can
/// assert the ceiling turn sent an empty tool list).
type ScriptedCall = (std::time::Instant, Vec<Message>, Vec<ToolSchema>);

/// A recording engine that replays a fixed sequence of scripted
/// [`Completion`]s and, per `chat` call, records the wall-clock [`Instant`]
/// plus the exact `&[Message]` it was invoked with (and the tool list).
/// This lets a test assert on the in-flight context ceiling (directive
/// appended, empty tool list on the final call) *and* order external
/// events (e.g. an older lease release) against the turn calls.
struct ScriptedEngine {
    responses: tokio::sync::Mutex<VecDeque<Completion>>,
    calls: tokio::sync::Mutex<Vec<ScriptedCall>>,
}

/// Build a [`Completion`] with the given content / tool calls and a small
/// deterministic token footprint (`prompt_tokens` reported, no cached /
/// completion tokens).
fn comp_metrics(content: &str, tool_calls: Vec<ToolCall>, prompt_tokens: u64) -> Completion {
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
            prompt_tokens: Some(prompt_tokens),
            cached_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
        },
    }
}

#[async_trait]
impl ChatEngine for ScriptedEngine {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        _stream: bool,
        _reasoning_effort: Option<&str>,
    ) -> Result<Completion, EngineError> {
        self.calls.lock().await.push((
            std::time::Instant::now(),
            messages.to_vec(),
            tools.to_vec(),
        ));
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
        None,
    )
    .await
    .unwrap();

    assert_eq!(res.final_text, "All done.");
    assert_eq!(res.status, "completed");
    assert_eq!(res.turns, 2);

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
async fn multi_turn_natural_completion_without_turn_budget() {
    let state = tmp_state();
    let logger = logger_for(&state);
    let recorder = std::sync::Arc::new(CallRecorder::default());
    // The model keeps calling tools and then produces its final answer naturally.
    let engine = RecordingEngine::new(
        vec![
            comp("", vec![tc("c1", "bash", "1")]),
            comp("", vec![tc("c2", "bash", "2")]),
            comp("", vec![tc("c3", "bash", "3")]),
            comp("Final answer after multiple steps.", Vec::new()),
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
        None,
    )
    .await
    .unwrap();

    assert_eq!(res.status, "completed");
    assert_eq!(res.final_text, "Final answer after multiple steps.");
    assert_eq!(res.turns, 4);
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
        None,
    )
    .await
    .unwrap();

    assert_eq!(res.final_text, "Salvaged summary.");
    assert_eq!(res.status, "completed");

    // The salvage prompt was injected into a call's messages.
    let calls = recorder.calls.lock().await;
    let salvage_injected = calls.iter().any(|c| {
        c.messages
            .iter()
            .any(|m| m.content.contains("[System: Salvage]"))
    });
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
            .any(|m| m.content.contains("[System: Loop Advisory]"))
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
    let res = run_session(&engine, &executor, &logger, "sys", "work", &tools, None)
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
        Some(&SessionOptions::with_state(state.clone())),
    )
    .await
    .unwrap();

    // The honest status survives the last-turn cooperative-landing branch.
    assert_eq!(
        res.status, "reasoning_budget_exhausted",
        "the reasoning ceiling must not be reclassified as cooperative landing"
    );
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
        has("[System: Salvage]"),
        "the non-coercive salvage prompt must be injected"
    );
    // (2) The salvage prompt is peer-empowering: it asks for grounded
    //     summaries, metrics, and blockers without coercive imperatives.
    let salvage_prompt = salvage_msgs
        .iter()
        .find(|m| m.content.contains("[System: Salvage]"))
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
        events
            .iter()
            .any(|e| e["type"] == "tool_result"
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
            .any(|m| m.content.contains("[System: Loop Advisory]"))
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
            .any(|m| m.content.contains("[System: Loop Advisory]"))
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

// --- workspace skeleton & custom spill threshold -----------------------

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
            content: "\u{3c}think\u{3e} reasoning 42 \u{3c}/think\u{3e} the answer is 42".into(),
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
    let engine2 = RecordingEngine::new(vec![comp("Second done.", Vec::new())], recorder2.clone());
    let executor2 = MockExecutor::new(Vec::new());
    let opts2 = SessionOptions::with_state(state.clone());
    let res2 = run_session(
        &engine2,
        &executor2,
        &logger2,
        "SYS",
        "second prompt",
        &[tool_schema("bash")],
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
        Some(&opts1),
    )
    .await
    .unwrap();

    // Second dispatch with a tiny max_context → history exceeds 75% →
    // roll over to a fresh [system, user] (no prior history resumed).
    let logger2 = EventLogger::new(&state, sid);
    let recorder2 = std::sync::Arc::new(CallRecorder::default());
    let engine2 = RecordingEngine::new(vec![comp("Second done.", Vec::new())], recorder2.clone());
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

/// An engine that records the `reasoning_effort` and the full message
/// vector passed to each `chat` call so tests can verify the watchdog
/// retry preserved the original effort and only appended a nudge at the
/// tail (the existing `RecordingEngine` records calls but not effort).
struct EffortRecordingEngine {
    responses: tokio::sync::Mutex<VecDeque<Completion>>,
    efforts: tokio::sync::Mutex<Vec<Option<String>>>,
    messages: tokio::sync::Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl ChatEngine for EffortRecordingEngine {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSchema],
        _stream: bool,
        effort: Option<&str>,
    ) -> Result<Completion, EngineError> {
        self.efforts.lock().await.push(effort.map(str::to_string));
        self.messages.lock().await.push(messages.to_vec());
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

/// The trailing user-message nudge the watchdog appends on a wall-clock
/// timeout stall (appended *after* the shared prefix, so the KV prefix
/// cache stays warm).
const TIMEOUT_RETRY_NUDGE: &str = "[System: Previous turn timed out. Do not deliberate; emit your next tool call or final response immediately.]";

/// The trailing user-message nudge the watchdog appends on a
/// reasoning-token-budget stall.
const BUDGET_RETRY_NUDGE: &str = "[System: Previous deliberation exceeded the single-turn reasoning budget. Be concise and emit your next tool call or final response immediately.]";

fn last_msg(messages: &[Message]) -> &str {
    messages
        .last()
        .map(|m| m.content.as_str())
        .unwrap_or_default()
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
        messages: tokio::sync::Mutex::new(Vec::new()),
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
    assert_eq!(*efforts, Vec::from([None]), "exactly one call, no retry");
    drop(efforts);
    let messages = engine.messages.lock().await;
    assert_eq!(messages.len(), 1, "exactly one call: no nudge was appended");
}

/// Token-budget trigger: a `length`/empty-tool-call turn that burned at
/// least the budget is retried once at the *original* effort with the
/// budget nudge appended at the tail — preserving the KV prefix cache.
#[tokio::test]
async fn watchdog_reasoning_budget_retry_preserves_prefix_and_effort() {
    let state = tmp_state();
    let logger = logger_for(&state);
    let engine = EffortRecordingEngine {
        responses: tokio::sync::Mutex::new(vec![stall_comp(20_000), ok_comp()].into()),
        efforts: tokio::sync::Mutex::new(Vec::new()),
        messages: tokio::sync::Mutex::new(Vec::new()),
    };
    let res = chat_with_watchdog(&engine, &[], &[], Some("high"), 0, 16_384, &logger, 1)
        .await
        .unwrap();
    // The retry's (non-stalling) completion is what comes back.
    assert_eq!(res.finish_reason.as_deref(), Some("stop"));
    let efforts = engine.efforts.lock().await;
    assert_eq!(
        *efforts,
        Vec::from([Some("high".into()), Some("high".into())]),
        "the retry must reuse the original effort (never 'low')"
    );
    // The stall was logged as a `turn_stall_retry` event.
    let events = logger.read_all();
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "turn_stall_retry" && e["reason"] == "reasoning_token_budget"),
        "expected a reasoning_token_budget stall event: {events:?}"
    );
    // The retry appended the budget nudge as the final message, leaving the
    // shared prefix (here: empty) untouched.
    let messages = engine.messages.lock().await;
    assert_eq!(messages.len(), 2, "primary + one retry");
    assert_eq!(
        last_msg(&messages[0]),
        "",
        "primary call sent the original (empty) prefix"
    );
    assert_eq!(
        last_msg(&messages[1]),
        BUDGET_RETRY_NUDGE,
        "the retry must append the budget nudge at the tail"
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
        messages: tokio::sync::Mutex::new(Vec::new()),
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
/// configured ceiling, the watchdog retries once at the *original* effort
/// with the timeout nudge appended at the tail — preserving the KV prefix
/// cache.
#[tokio::test]
async fn watchdog_timeout_retry_preserves_prefix_and_effort() {
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
            messages: &[Message],
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
            self.inner.messages.lock().await.push(messages.to_vec());
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
        messages: tokio::sync::Mutex::new(Vec::new()),
    };
    let engine = SlowThenOkEngine { inner };
    let res = chat_with_watchdog(&engine, &[], &[], Some("high"), 1, 0, &logger, 1)
        .await
        .unwrap();
    assert_eq!(res.finish_reason.as_deref(), Some("stop"));
    // The timed-out primary recorded its effort up-front before being
    // dropped, and the retry reuses the *same* effort (never "low").
    let events = logger.read_all();
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "turn_stall_retry" && e["reason"] == "timeout"),
        "expected a timeout stall event: {events:?}"
    );
    let efforts = engine.inner.efforts.lock().await;
    assert_eq!(
        *efforts,
        Vec::from([Some("high".into()), Some("high".into())]),
        "the retry must reuse the original effort"
    );
    // The retry appended the timeout nudge as the final message, leaving
    // the shared prefix (here: empty) untouched on both calls.
    let messages = engine.inner.messages.lock().await;
    assert_eq!(messages.len(), 2, "primary + one retry were issued");
    assert_eq!(
        last_msg(&messages[0]),
        "",
        "the primary sent the original (empty) prefix"
    );
    assert_eq!(
        last_msg(&messages[1]),
        TIMEOUT_RETRY_NUDGE,
        "the retry must append the timeout nudge at the tail"
    );
}

/// Finding 5 — true cooperative yield handoff, end-to-end (overcommit-only
/// gate).
///
/// The pool is token-budgeted at 100. An *older* worker claims one slot at
/// 60 context tokens and holds it ~80ms. The session runs as a *younger*
/// worker (a 2nd slot) that, after its first turn, records 60 context
/// tokens of its own — pushing the pool to 120 > 100 (overcommit). The
/// yield gate then fires: the runner releases its slot and re-acquires
/// through the standard token-budgeted acquire, which (the pool still
/// overcommitted) waits in the FIFO queue until the older lease frees
/// capacity, then runs Turn 2. We prove the handoff by asserting the older
/// release instant lands strictly between the Turn-1 and Turn-2 engine
/// calls.
#[tokio::test]
async fn cooperative_yield_waits_for_release_before_next_turn() {
    use crate::task::semaphore::TaskSemaphore;

    let state = tmp_state();
    let state_root = state.root().to_path_buf();
    let logger = logger_for(&state);
    // Two slots so the older and younger worker can hold one each; the
    // short poll interval keeps the re-acquire wait tight. The pool is
    // budgeted at 100 so the yield gate (driven by overcommit) can fire.
    let sem = TaskSemaphore::with_poll_interval(&state, 2, 5).with_token_budget(100);

    // The older worker: claim a slot at 60 context tokens (under the 100
    // budget, so admitted freely), hold it ~80ms, then release it.
    let older_release_at: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let rel = older_release_at.clone();
    let sem_old = sem.clone();
    let older = tokio::spawn(async move {
        let lease = sem_old.acquire_with_tokens("task_old", 60).await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let _ = sem_old.release(&lease);
        *rel.lock().unwrap() = Some(std::time::Instant::now());
    });

    // Give the older worker a clear head start before the younger claims.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The younger worker (this session) claims the 2nd slot at 0 (pool
    // committed is 60, under budget, so it is admitted).
    let younger = sem.acquire_with_tokens("task_young", 0).await;
    let ctx = LeaseYieldContext {
        sem: std::sync::Arc::new(sem.clone()),
        lease: std::sync::Arc::new(std::sync::Mutex::new(younger)),
        task_id: "task_young".into(),
    };
    let mut options = SessionOptions::with_state(state).with_lease_context(ctx);
    // A large `max_context` so the 90% in-flight ceiling never trips and
    // the pool-overcommit yield path is what we exercise here.
    options.max_context = 100_000;

    // Turn 1: a tool call (so the turn is not final). The engine reports
    // 60 prompt tokens, which the runner records on the younger's lease →
    // pool committed becomes 60 (old) + 60 (young) = 120 > 100 → overcommit
    // → yield. Turn 2: a final (tool-free) completion.
    let engine = ScriptedEngine {
        responses: tokio::sync::Mutex::new(
            vec![
                comp_metrics("", vec![tc("c1", "bash", "{\"cmd\":\"ls\"}")], 60),
                comp_metrics("All done.", Vec::new(), 10),
            ]
            .into(),
        ),
        calls: tokio::sync::Mutex::new(Vec::new()),
    };
    let executor = MockExecutor::new(vec!["file1\nfile2".into()]);

    let res = run_session(
        &engine,
        &executor,
        &logger,
        "sys",
        "do the thing",
        &[tool_schema("bash")],
        Some(&options),
    )
    .await
    .unwrap();
    let _ = older.await;

    // The session completed its 2 turns.
    assert_eq!(res.status, "completed");
    assert_eq!(res.turns, 2);

    // The yield gate fired (the overcommitted younger worker on Turn 1)
    // and was logged.
    let events = logger.read_all();
    assert!(
        events.iter().any(|e| e["type"] == "cooperative_yield"),
        "expected a cooperative_yield event: {events:?}"
    );

    // The older release must land strictly between the Turn-1 and Turn-2
    // calls — i.e. the younger worker actually waited for capacity (it did
    // not just drop and immediately re-claim its own slot).
    let calls = engine.calls.lock().await.clone();
    assert_eq!(
        calls.len(),
        2,
        "expected exactly two engine calls: {calls:?}"
    );
    let older_release_at = older_release_at.lock().unwrap();
    let Some(older_release_at) = older_release_at.as_ref() else {
        panic!("older worker never released its lease");
    };
    assert!(
        calls[0].0 < *older_release_at && *older_release_at <= calls[1].0,
        "older release must be waited on between turn 1 ({:?}) and turn 2 ({:?})",
        calls[0].0,
        calls[1].0
    );
    let _ = std::fs::remove_dir_all(&state_root);
}

#[tokio::test]
async fn in_flight_context_ceiling_disables_tools_and_lands_final() {
    let state = tmp_state();
    let state_root = state.root().to_path_buf();
    let logger = logger_for(&state);

    // 3_700 chars / 4 = 925 tokens > 900 (90% of 1000) — over the ceiling
    // before the first turn.
    let big_prompt = "x".repeat(3_700);

    let engine = ScriptedEngine {
        responses: tokio::sync::Mutex::new(vec![comp_metrics("Done.", Vec::new(), 950)].into()),
        calls: tokio::sync::Mutex::new(Vec::new()),
    };
    let executor = MockExecutor::new(Vec::new());

    let mut options = SessionOptions::with_state(state);
    options.max_context = 1_000;

    let res = run_session(
        &engine,
        &executor,
        &logger,
        "sys",
        &big_prompt,
        &[tool_schema("bash")],
        Some(&options),
    )
    .await
    .unwrap();

    assert_eq!(res.status, "completed");
    assert_eq!(res.final_text, "Done.");

    let calls = engine.calls.lock().await.clone();
    assert_eq!(calls.len(), 1, "ceiling must land in one final call");
    let (_, msgs, tools_sent) = &calls[0];
    let last_user = msgs
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .expect("a user message must be present");
    assert!(
        last_user
            .content
            .contains("Working context budget has reached 90%"),
        "expected ceiling directive, got: {last_user:?}"
    );
    assert!(
        tools_sent.is_empty(),
        "tool dispatch must be disabled on the ceiling turn, but sent {tools_sent:?}"
    );

    let events = logger.read_all();
    assert!(
        events.iter().any(|e| e["type"] == "context_ceiling"),
        "expected a context_ceiling event: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&state_root);
}

#[tokio::test]
async fn in_flight_context_ceiling_uses_engine_prompt_tokens() {
    let state = tmp_state();
    let state_root = state.root().to_path_buf();
    let logger = logger_for(&state);

    // Text is tiny (~10 chars), well below chars/4 ceiling, but Turn 1
    // engine metrics report 910 prompt tokens >= 900 (90% of 1000).
    let engine = ScriptedEngine {
        responses: tokio::sync::Mutex::new(
            vec![
                comp_metrics("", vec![tc("c1", "bash", r#"{"cmd":"ls"}"#)], 910),
                comp_metrics("Final synthesis.", Vec::new(), 940),
            ]
            .into(),
        ),
        calls: tokio::sync::Mutex::new(Vec::new()),
    };
    let executor = MockExecutor::new(vec!["ok".into()]);

    let mut options = SessionOptions::with_state(state);
    options.max_context = 1_000;

    let res = run_session(
        &engine,
        &executor,
        &logger,
        "sys",
        "go",
        &[tool_schema("bash")],
        Some(&options),
    )
    .await
    .unwrap();

    assert_eq!(res.status, "completed");
    assert_eq!(res.turns, 2);
    assert_eq!(res.final_text, "Final synthesis.");

    let calls = engine.calls.lock().await.clone();
    assert_eq!(calls.len(), 2);
    assert!(
        !calls[0].2.is_empty(),
        "turn 1 should carry tools before ceiling trips"
    );
    assert!(
        calls[1].2.is_empty(),
        "turn 2 must disable tools once engine prompt_tokens >= 90%"
    );
    let _ = std::fs::remove_dir_all(&state_root);
}

#[tokio::test]
async fn in_flight_context_ceiling_blocks_slipped_tool_calls() {
    let state = tmp_state();
    let state_root = state.root().to_path_buf();
    let logger = logger_for(&state);

    let big_prompt = "x".repeat(3_700);
    // Simulate a non-compliant engine that returns a tool call even when
    // tools=[] was sent.
    let engine = ScriptedEngine {
        responses: tokio::sync::Mutex::new(
            vec![comp_metrics(
                "Partial synthesis.",
                vec![tc("c1", "bash", r#"{"cmd":"ls"}"#)],
                950,
            )]
            .into(),
        ),
        calls: tokio::sync::Mutex::new(Vec::new()),
    };
    let executor = MockExecutor::new(vec!["should-not-run".into()]);

    let mut options = SessionOptions::with_state(state);
    options.max_context = 1_000;

    let res = run_session(
        &engine,
        &executor,
        &logger,
        "sys",
        &big_prompt,
        &[tool_schema("bash")],
        Some(&options),
    )
    .await
    .unwrap();

    assert_eq!(res.status, "completed");
    assert_eq!(res.turns, 1);
    assert_eq!(res.final_text, "Partial synthesis.");
    assert!(
        executor.calls().await.is_empty(),
        "slipped tool calls on ceiling turn must never execute"
    );
    let _ = std::fs::remove_dir_all(&state_root);
}
