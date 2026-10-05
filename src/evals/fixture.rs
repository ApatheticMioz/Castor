//! Strict typed parsing of eval fixtures: `task.toml` and `trace.jsonl`.
//!
//! Formats are defined in `castor/evals/EVALS.md` (§2 task format, §3 trace
//! format). Both parsers are strict: unknown fields are denied, and any
//! malformed input yields a typed error naming the offending file/line.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// task.toml
// ---------------------------------------------------------------------------

/// One deterministic scorer check (`[[scorer.checks]]` in `task.toml`).
///
/// `kind` selects which of the three optional payload fields is present:
/// `exec` → `cmd`/`expect_exit`, `file` → `path`/`expect_sha256` or
/// `expect_contains`, `trace` → `assert`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub expect_exit: Option<i64>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub expect_sha256: Option<String>,
    #[serde(default)]
    pub expect_contains: Option<String>,
    #[serde(default)]
    pub assert: Option<String>,
}

/// The `[scorer]` table: an ordered list of check-ids' definitions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scorer {
    pub checks: Vec<Check>,
}

/// Parsed `task.toml`. Field set mirrors the real fixtures exactly; unknown
/// keys are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    pub id: String,
    pub prompt: String,
    /// Optional setup script (relative to the task dir).
    #[serde(default)]
    pub setup: Option<String>,
    pub timeout_s: u64,
    pub tags: Vec<String>,
    pub scorer: Scorer,
}

/// Typed errors for loading a task fixture.
#[derive(Debug, Error)]
pub enum TaskError {
    /// A required fixture file is missing; the path is named.
    #[error("missing required file: {0}")]
    MissingFile(PathBuf),
    /// I/O failure reading a fixture file.
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `task.toml` does not parse (syntax, unknown field, bad type).
    #[error("invalid task.toml at {path}: {message}")]
    Toml { path: PathBuf, message: String },
}

/// A task fixture directory (`evals/tasks/<id>/`).
#[derive(Debug)]
pub struct Task {
    pub config: TaskConfig,
    /// The task directory this task was loaded from.
    pub dir: PathBuf,
}

impl Task {
    /// Load a task from its fixture directory.
    ///
    /// Required files: `task.toml` and `trace.jsonl` (the golden Tier-A
    /// trace). `known-fail/trace.jsonl` is optional (the canary task ships
    /// without one). Missing required files yield [`TaskError::MissingFile`]
    /// naming the exact path.
    pub fn load(dir: &Path) -> Result<Self, TaskError> {
        let toml_path = dir.join("task.toml");
        if !toml_path.is_file() {
            return Err(TaskError::MissingFile(toml_path));
        }
        let trace_path = dir.join("trace.jsonl");
        if !trace_path.is_file() {
            return Err(TaskError::MissingFile(trace_path));
        }

        let raw = fs::read_to_string(&toml_path).map_err(|e| TaskError::Io {
            path: toml_path.clone(),
            source: e,
        })?;
        let config: TaskConfig = toml::from_str(&raw).map_err(|e| TaskError::Toml {
            path: toml_path,
            message: e.to_string(),
        })?;

        Ok(Self {
            config,
            dir: dir.to_path_buf(),
        })
    }

    /// The golden (expected-pass) trace of this task.
    pub fn golden_trace(&self) -> Result<Vec<TraceStep>, TraceError> {
        Trace::load(&self.dir.join("trace.jsonl"))
    }

    /// The known-fail variant trace, if the task ships one.
    pub fn known_fail_trace(&self) -> Result<Option<Vec<TraceStep>>, TraceError> {
        let path = self.dir.join("known-fail").join("trace.jsonl");
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(Trace::load(&path)?))
    }
}

// ---------------------------------------------------------------------------
// trace.jsonl
// ---------------------------------------------------------------------------

/// A tool call reference embedded in an `assistant_message` event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ToolCallRef {
    pub id: String,
    pub r#type: String,
    pub function: ToolCallFunction,
}

/// The function descriptor inside a [`ToolCallRef`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallFunction {
    pub name: String,
    /// Raw JSON-encoded arguments string, exactly as the model emitted it.
    pub arguments: String,
}

/// One line of a `trace.jsonl` fixture: a single session event.
///
/// The `type` discriminator selects which payload fields are present; all
/// fields are optional and unknown fields are denied, so a line is valid
/// for exactly the event type it claims. JSON keys are camelCase, matching
/// the real Castor session event log.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "camelCase")]
pub enum TraceStep {
    #[serde(rename = "session_start", rename_all = "camelCase")]
    SessionStart {
        timestamp: String,
        session_id: String,
        harness: String,
        version: String,
        cwd: String,
        prompt: String,
    },
    #[serde(rename = "assistant_message", rename_all = "camelCase")]
    AssistantMessage {
        timestamp: String,
        session_id: String,
        content: String,
        #[serde(default)]
        tool_calls: Vec<ToolCallRef>,
    },
    #[serde(rename = "tool_call", rename_all = "camelCase")]
    ToolCall {
        timestamp: String,
        session_id: String,
        tool_call_id: String,
        name: String,
        args: serde_json::Value,
    },
    #[serde(rename = "tool_result", rename_all = "camelCase")]
    ToolResult {
        timestamp: String,
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        is_error: bool,
        latency_ms: u64,
    },
    #[serde(rename = "tool_output_spilled", rename_all = "camelCase")]
    ToolOutputSpilled {
        timestamp: String,
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        bytes: u64,
        path: String,
    },
    #[serde(rename = "session_end", rename_all = "camelCase")]
    SessionEnd {
        timestamp: String,
        session_id: String,
        status: String,
        turns_taken: u64,
        continuations_injected: u64,
        duration_ms: u64,
        total_completion_tokens: u64,
    },
}

/// Typed errors for loading a trace fixture.
#[derive(Debug, Error)]
pub enum TraceError {
    /// The trace file does not exist; the path is named.
    #[error("missing trace file: {0}")]
    MissingFile(PathBuf),
    /// I/O failure reading the trace file.
    #[error("io error reading {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    /// A line is not valid JSON or does not match the event schema; the
    /// 1-indexed line number is named.
    #[error("malformed trace line {line} in {path}: {message}")]
    MalformedLine {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

/// A parsed `trace.jsonl` fixture.
pub struct Trace {
    pub steps: Vec<TraceStep>,
    pub path: PathBuf,
}

impl Trace {
    /// Load a trace fixture: one JSON object per line.
    ///
    /// Blank lines are skipped. Any line that fails to parse (bad JSON,
    /// unknown field, wrong type) yields [`TraceError::MalformedLine`] with
    /// the 1-indexed line number.
    pub fn load(path: &Path) -> Result<Vec<TraceStep>, TraceError> {
        let raw = fs::read_to_string(path).map_err(|e| TraceError::Io(path.to_path_buf(), e))?;
        let mut steps = Vec::new();
        for (idx, line) in raw.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let step: TraceStep =
                serde_json::from_str(trimmed).map_err(|e| TraceError::MalformedLine {
                    path: path.to_path_buf(),
                    line: line_no,
                    message: e.to_string(),
                })?;
            steps.push(step);
        }
        Ok(steps)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("evals")
            .join("tasks")
    }

    #[test]
    fn loads_real_fix_failing_test_task() {
        let dir = fixture_dir().join("fix-failing-test");
        let task = Task::load(&dir).expect("real fixture must load");

        let cfg = &task.config;
        assert_eq!(cfg.id, "fix-failing-test");
        assert!(
            cfg.prompt.contains("Fix the bug in src/sum.js"),
            "prompt: {}",
            cfg.prompt
        );
        assert_eq!(cfg.setup.as_deref(), Some("setup.sh"));
        assert_eq!(cfg.timeout_s, 120);
        assert_eq!(cfg.tags, vec!["tier-a", "F2"]);

        assert_eq!(cfg.scorer.checks.len(), 3);
        assert_eq!(cfg.scorer.checks[0].id, "tests-green");
        assert_eq!(cfg.scorer.checks[0].kind, "exec");
        assert_eq!(
            cfg.scorer.checks[0].cmd.as_deref(),
            Some("node --test \"test/*.test.js\"")
        );
        assert_eq!(cfg.scorer.checks[0].expect_exit, Some(0));

        let file_check = &cfg.scorer.checks[1];
        assert_eq!(
            (file_check.id.as_str(), file_check.kind.as_str()),
            ("test-file-untouched", "file")
        );
        assert_eq!(file_check.path.as_deref(), Some("test/sum.test.js"));
        assert!(file_check.expect_sha256.is_some());
        assert!(file_check.expect_contains.is_none());

        let trace_check = &cfg.scorer.checks[2];
        assert_eq!(
            (trace_check.id.as_str(), trace_check.kind.as_str()),
            ("no-dropped-toolcall", "trace")
        );
        assert_eq!(
            trace_check.assert.as_deref(),
            Some("every_tool_call_has_result")
        );
    }

    #[test]
    fn loads_real_fix_failing_test_golden_trace() {
        let path = fixture_dir().join("fix-failing-test").join("trace.jsonl");
        let steps = Trace::load(&path).expect("golden trace must load");
        assert!(!steps.is_empty(), "expected at least one step");
        assert_eq!(steps.len(), 12);

        // First line is a session_start with the task prompt.
        match &steps[0] {
            TraceStep::SessionStart {
                prompt, harness, ..
            } => {
                assert_eq!(harness, "Castor");
                assert!(prompt.contains("Fix the bug in src/sum.js"));
            }
            other => panic!("expected session_start first, got {other:?}"),
        }

        // An assistant_message carries the raw model response + tool call refs.
        match &steps[1] {
            TraceStep::AssistantMessage {
                content,
                tool_calls,
                ..
            } => {
                assert!(!content.is_empty());
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].function.name, "read_file");
                assert!(
                    tool_calls[0]
                        .function
                        .arguments
                        .contains("test/sum.test.js")
                );
            }
            other => panic!("expected assistant_message, got {other:?}"),
        }

        // A tool_call carries parsed args.
        match &steps[2] {
            TraceStep::ToolCall { name, args, .. } => {
                assert_eq!(name, "read_file");
                assert_eq!(args["path"], "test/sum.test.js");
            }
            other => panic!("expected tool_call, got {other:?}"),
        }

        // A tool_result carries the result payload.
        match &steps[3] {
            TraceStep::ToolResult {
                tool_name,
                result,
                is_error,
                ..
            } => {
                assert_eq!(tool_name, "read_file");
                assert!(!is_error);
                assert!(result.is_some());
            }
            other => panic!("expected tool_result, got {other:?}"),
        }

        // Last line is session_end.
        match steps.last().unwrap() {
            TraceStep::SessionEnd {
                status,
                turns_taken,
                ..
            } => {
                assert_eq!(status, "completed");
                assert_eq!(*turns_taken, 3);
            }
            other => panic!("expected session_end last, got {other:?}"),
        }
    }

    #[test]
    fn loads_real_fix_failing_test_known_fail_trace() {
        let path = fixture_dir()
            .join("fix-failing-test")
            .join("known-fail")
            .join("trace.jsonl");
        let steps = Trace::load(&path).expect("known-fail trace must load");
        assert_eq!(steps.len(), 9);
        // The known-fail run edits the test file — the scorer must catch it.
        let edited_test = steps.iter().any(|s| {
            matches!(
                s,
                TraceStep::ToolCall { name, args, .. }
                    if name == "edit_file" && args["path"] == "test/sum.test.js"
            )
        });
        assert!(edited_test, "known-fail trace must edit the test file");
    }

    #[test]
    fn malformed_trace_line_yields_typed_error_with_line_number() {
        let dir = std::env::temp_dir().join("castor-evals-test-malformed");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        // Line 1 valid, line 2 has an unknown field, line 3 valid.
        fs::write(
            &path,
            r#"{"timestamp":"t","sessionId":"s","type":"session_start","harness":"Castor","version":"1","cwd":"/w","prompt":"p"}
{"timestamp":"t","sessionId":"s","type":"tool_call","toolCallId":"c","name":"bash","args":{},"bogus":1}
{"timestamp":"t","sessionId":"s","type":"session_end","status":"completed","turnsTaken":1,"continuationsInjected":0,"durationMs":1,"totalCompletionTokens":1}
"#,
        )
        .unwrap();

        let err = Trace::load(&path).unwrap_err();
        match err {
            TraceError::MalformedLine { line, message, .. } => {
                assert_eq!(line, 2, "error must name line 2, got: {message}");
                assert!(message.contains("bogus"), "{message}");
            }
            other => panic!("expected MalformedLine, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_json_line_yields_typed_error_with_line_number() {
        let dir = std::env::temp_dir().join("castor-evals-test-badjson");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        fs::write(&path, "not json at all\n").unwrap();

        let err = Trace::load(&path).unwrap_err();
        match err {
            TraceError::MalformedLine { line, .. } => assert_eq!(line, 1),
            other => panic!("expected MalformedLine, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_task_toml_yields_typed_error_naming_path() {
        let dir = std::env::temp_dir().join("castor-evals-test-missing-toml");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("trace.jsonl"), "{}\n").unwrap();

        let err = Task::load(&dir).unwrap_err();
        match err {
            TaskError::MissingFile(p) => {
                assert!(p.ends_with("task.toml"), "{p:?}");
                assert!(p.starts_with(&dir), "{p:?}");
            }
            other => panic!("expected MissingFile, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_trace_yields_typed_error_naming_path() {
        let dir = std::env::temp_dir().join("castor-evals-test-missing-trace");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("task.toml"),
            "id = \"x\"\nprompt = \"p\"\ntimeout_s = 1\ntags = []\n[scorer]\nchecks = []\n",
        )
        .unwrap();

        let err = Task::load(&dir).unwrap_err();
        match err {
            TaskError::MissingFile(p) => {
                assert!(p.ends_with("trace.jsonl"), "{p:?}");
            }
            other => panic!("expected MissingFile, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_task_toml_field_is_rejected() {
        let dir = std::env::temp_dir().join("castor-evals-test-unknown-field");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("task.toml"),
            "id = \"x\"\nprompt = \"p\"\ntimeout_s = 1\ntags = []\nextra = 1\n[scorer]\nchecks = []\n",
        )
        .unwrap();
        fs::write(dir.join("trace.jsonl"), "{}\n").unwrap();

        let err = Task::load(&dir).unwrap_err();
        match err {
            TaskError::Toml { message, .. } => {
                assert!(message.contains("extra"), "{message}");
            }
            other => panic!("expected Toml error, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn all_real_task_fixtures_load() {
        let tasks = fixture_dir();
        for entry in fs::read_dir(&tasks).unwrap() {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            let task = Task::load(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            let golden = task
                .golden_trace()
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            assert!(!golden.is_empty(), "{}: empty golden trace", dir.display());
            // Every task except the canary ships a known-fail variant.
            let known_fail = task
                .known_fail_trace()
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            if dir.file_name().unwrap() != "canary-hard" {
                assert!(
                    known_fail.is_some(),
                    "{}: missing known-fail",
                    dir.display()
                );
            }
        }
    }
}
