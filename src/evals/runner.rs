//! Eval runner: glues the fixture, replay engine, and deterministic scorer
//! together into a single `run_task` entry point.
//!
//! `run_task(task_dir, variant)`:
//! 1. Loads the task fixture (`task.toml` + the selected trace).
//! 2. Copies the `fixture/` tree into a fresh temp workspace.
//! 3. Applies the optional `setup.sh` (no-op if declared-but-missing).
//! 4. Replays the recorded trace through the *real* session loop
//!    (`run_session` with a [`ReplayEngine`] + [`FsExecutor`] rooted at the
//!    temp workspace), materializing the recorded file mutations.
//! 5. Scores the post-run workspace + recorded trace against the task's
//!    deterministic checks (`exec` / `file` / `trace`).
//!
//! Outcome discipline (EVALS.md §4): `Error` is strictly reserved for
//! harness-internal failures (missing fixture, trace parse failure, engine
//! trait error, setup failure). Scorer check failures are `Fail`, never
//! `Error`. A check that cannot be evaluated (unknown kind/assert, exec
//! spawn/policy failure) is itself an `Error`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::evals::fixture::{Check, Task, TaskConfig, TraceStep};
use crate::evals::replay::ReplayEngine;
use crate::runner::events::EventLogger;
use crate::runner::run_session;
use crate::state::StateDir;
use crate::tools::fs::FsExecutor;
use crate::tools::shell;

/// Which recorded trace variant to replay and score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The golden (expected-pass) trace at `trace.jsonl`.
    Golden,
    /// The known-fail (expected-fail) trace at `known-fail/trace.jsonl`.
    KnownFail,
}

impl std::fmt::Display for Variant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Variant::Golden => write!(f, "golden"),
            Variant::KnownFail => write!(f, "known-fail"),
        }
    }
}

/// The status of a single scorer check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check failed (a scorer-level failure — the agent regressed).
    Fail(String),
    /// The check could not be evaluated (harness-internal failure).
    Error(String),
}

/// The result of a single scorer check.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub id: String,
    pub kind: String,
    pub status: CheckStatus,
}

/// The overall outcome of an eval run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every check passed.
    Pass,
    /// One or more checks failed (scorer caught a regression).
    Fail(Vec<String>),
    /// A harness-internal failure blocked the run.
    Error(String),
}

/// The report produced by [`run_task`].
#[derive(Debug, Clone)]
pub struct EvalReport {
    pub task_id: String,
    pub variant: Variant,
    pub outcome: Outcome,
    pub checks: Vec<CheckResult>,
}

/// A monotonically increasing counter for unique temp-dir names.
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_unique() -> u64 {
    UNIQUE_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Run a single eval task: replay the selected trace variant and score it.
///
/// This is a synchronous entry point. It creates its own current-thread
/// tokio runtime to drive the async session loop, and relies on
/// [`crate::tools::shell::run`] (which builds its own runtime) for the exec
/// checks. It is intended to be called from a plain `#[test]` (not from
/// inside an existing runtime).
pub fn run_task(task_dir: &Path, variant: Variant) -> EvalReport {
    let dir_name = task_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    // 1. Load the task fixture.
    let task = match Task::load(task_dir) {
        Ok(t) => t,
        Err(e) => return error_report(dir_name, variant, format!("load task: {e}")),
    };
    let config = &task.config;
    let task_id = config.id.clone();

    // 2. Load the selected trace variant.
    let steps = match variant {
        Variant::Golden => match task.golden_trace() {
            Ok(s) => s,
            Err(e) => return error_report(task_id, variant, format!("load golden trace: {e}")),
        },
        Variant::KnownFail => match task.known_fail_trace() {
            Ok(Some(s)) => s,
            Ok(None) => {
                return error_report(task_id, variant, "no known-fail trace present".to_string());
            }
            Err(e) => return error_report(task_id, variant, format!("load known-fail trace: {e}")),
        },
    };
    // Keep a copy for the trace assertions (the original is moved into the
    // replay engine below).
    let trace_steps = steps.clone();

    // 3. Copy the fixture tree into a fresh temp workspace.
    let workspace = match make_temp_workspace(task_dir) {
        Ok(w) => w,
        Err(e) => return error_report(task_id, variant, e),
    };

    // 4. Apply the optional setup script.
    if let Err(e) = apply_setup(task_dir, config, &workspace) {
        let _ = fs::remove_dir_all(&workspace);
        return error_report(task_id, variant, format!("setup: {e}"));
    }

    // 5. Build the replay engine and the filesystem executor.
    let engine = ReplayEngine::new(steps);
    let executor = match FsExecutor::new(&workspace) {
        Ok(e) => e,
        Err(e) => {
            let _ = fs::remove_dir_all(&workspace);
            return error_report(task_id, variant, format!("build executor: {e}"));
        }
    };

    // 6. Run the real session loop, materializing the recorded mutations.
    let state_root = std::env::temp_dir().join(format!(
        "castor-eval-state-{}-{}",
        std::process::id(),
        next_unique()
    ));
    let state = StateDir::new(&state_root);
    let _ = fs::create_dir_all(state.sessions());
    let logger = EventLogger::new(&state, &task_id);

    let session = {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = fs::remove_dir_all(&workspace);
                let _ = fs::remove_dir_all(&state_root);
                return error_report(task_id, variant, format!("build session runtime: {e}"));
            }
        };
        rt.block_on(run_session(
            &engine,
            &executor,
            &logger,
            "You are a precise, sandboxed coding agent.",
            &config.prompt,
            &[],
            0, // use the default turn budget
            Some(&crate::runner::SessionOptions::with_workspace(
                workspace.clone(),
            )),
        ))
    };
    let _ = fs::remove_dir_all(&state_root);

    match session {
        Ok(_) => {}
        Err(e) => {
            let _ = fs::remove_dir_all(&workspace);
            return error_report(task_id, variant, format!("session: {e}"));
        }
    }

    // 7. Score the post-run workspace and the recorded trace.
    let timeout = Duration::from_secs(config.timeout_s.max(1));
    let checks = score(config, &workspace, &trace_steps, timeout);

    // 8. Determine the overall outcome.
    let outcome = determine_outcome(&checks);

    // 9. Clean up the temp workspace.
    let _ = fs::remove_dir_all(&workspace);

    EvalReport {
        task_id,
        variant,
        outcome,
        checks,
    }
}

/// Build an `EvalReport` representing a harness-internal failure.
fn error_report(task_id: String, variant: Variant, reason: String) -> EvalReport {
    EvalReport {
        task_id,
        variant,
        outcome: Outcome::Error(reason),
        checks: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Workspace / setup
// ---------------------------------------------------------------------------

/// Copy the task's `fixture/` tree into a fresh temp workspace.
fn make_temp_workspace(task_dir: &Path) -> Result<PathBuf, String> {
    let fixture = task_dir.join("fixture");
    if !fixture.is_dir() {
        return Err(format!("missing fixture dir: {}", fixture.display()));
    }
    let workspace = std::env::temp_dir().join(format!(
        "castor-eval-{}-{}",
        std::process::id(),
        next_unique()
    ));
    let _ = fs::remove_dir_all(&workspace);
    copy_dir(&fixture, &workspace)
        .map_err(|e| format!("copy fixture to {}: {e}", workspace.display()))?;
    Ok(workspace)
}

/// Recursively copy a directory tree.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Apply the task's optional setup script against the temp workspace.
///
/// A declared-but-missing setup script is a no-op (the fixture is
/// self-contained). A present script that exits non-zero is a harness
/// failure.
fn apply_setup(task_dir: &Path, config: &TaskConfig, workspace: &Path) -> Result<(), String> {
    let Some(setup_rel) = config.setup.as_deref() else {
        return Ok(());
    };
    let setup_path = task_dir.join(setup_rel);
    if !setup_path.is_file() {
        return Ok(()); // declared but missing → no-op
    }
    let output = std::process::Command::new("bash")
        .arg(&setup_path)
        .current_dir(workspace)
        .output()
        .map_err(|e| format!("spawn {}: {e}", setup_path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} exited {}: {}",
            setup_path.display(),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Score all of the task's checks against the post-run workspace and the
/// recorded trace.
fn score(
    config: &TaskConfig,
    workspace: &Path,
    steps: &[TraceStep],
    timeout: Duration,
) -> Vec<CheckResult> {
    config
        .scorer
        .checks
        .iter()
        .map(|check| {
            let status = match check.kind.as_str() {
                "exec" => score_exec(check, workspace, timeout),
                "file" => score_file(check, workspace),
                "trace" => score_trace(check, steps),
                other => CheckStatus::Error(format!("unknown check kind: {other}")),
            };
            CheckResult {
                id: check.id.clone(),
                kind: check.kind.clone(),
                status,
            }
        })
        .collect()
}

/// Score an `exec` check by running its command against the workspace.
fn score_exec(check: &Check, workspace: &Path, timeout: Duration) -> CheckStatus {
    let Some(cmd) = check.cmd.as_deref() else {
        return CheckStatus::Error("exec check missing `cmd`".to_string());
    };
    let expected = check.expect_exit.unwrap_or(0) as i32;
    match shell::run(cmd, workspace, timeout) {
        Ok(out) => {
            if out.exit_code == expected {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail(format!(
                    "expected exit {expected}, got {} (truncated={})",
                    out.exit_code, out.truncated
                ))
            }
        }
        Err(e) => CheckStatus::Error(format!("exec check error: {e}")),
    }
}

/// Score a `file` check by reading the file from the workspace.
fn score_file(check: &Check, workspace: &Path) -> CheckStatus {
    let Some(path) = check.path.as_deref() else {
        return CheckStatus::Error("file check missing `path`".to_string());
    };
    let file_path = workspace.join(path);
    let content = match fs::read_to_string(&file_path) {
        Ok(c) => c,
        Err(_) => return CheckStatus::Fail(format!("file not found: {path}")),
    };
    if let Some(expected_sha) = check.expect_sha256.as_deref() {
        let actual = sha256_hex(content.as_bytes());
        if actual == expected_sha {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail(format!(
                "sha256 mismatch: expected {expected_sha}, got {actual}"
            ))
        }
    } else if let Some(expected_contains) = check.expect_contains.as_deref() {
        if content.contains(expected_contains) {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail(format!("file {path} does not contain: {expected_contains}"))
        }
    } else {
        CheckStatus::Error(
            "file check has neither `expect_sha256` nor `expect_contains`".to_string(),
        )
    }
}

/// Score a `trace` check by evaluating its named assertion against the
/// recorded trace.
fn score_trace(check: &Check, steps: &[TraceStep]) -> CheckStatus {
    let Some(assert) = check.assert.as_deref() else {
        return CheckStatus::Error("trace check missing `assert`".to_string());
    };
    match assert {
        "every_tool_call_has_result" => trace_every_tool_call_has_result(steps),
        "final_message_nonempty" => trace_final_message_nonempty(steps),
        "no_tool_output_spilled" => trace_no_tool_output_spilled(steps),
        "no_write_outside_workdir" => trace_no_write_outside_workdir(steps),
        "final_message_mentions_workspace" => trace_final_message_mentions_workspace(steps),
        "read_file_before_edit_file" => trace_read_file_before_edit_file(steps),
        "edit_file_before_bash_verify" => trace_edit_file_before_bash_verify(steps),
        "bash_verify_present" => trace_bash_verify_present(steps),
        "edit_file_count_eq_1" => trace_edit_file_count_eq_1(steps),
        "no_consecutive_identical_failed_toolcall" => {
            trace_no_consecutive_identical_failed_toolcall(steps)
        }
        other => CheckStatus::Error(format!("unknown trace assertion: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Trace assertions
// ---------------------------------------------------------------------------

/// Extract the `tool_call` events in trace order as `(name, args, id)`.
fn tool_calls(steps: &[TraceStep]) -> Vec<(String, Value, String)> {
    steps
        .iter()
        .filter_map(|s| match s {
            TraceStep::ToolCall {
                name,
                args,
                tool_call_id,
                ..
            } => Some((name.clone(), args.clone(), tool_call_id.clone())),
            _ => None,
        })
        .collect()
}

/// Extract the `tool_result` events in trace order as `(id, is_error)`.
fn tool_results(steps: &[TraceStep]) -> Vec<(String, bool)> {
    steps
        .iter()
        .filter_map(|s| match s {
            TraceStep::ToolResult {
                tool_call_id,
                is_error,
                ..
            } => Some((tool_call_id.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// The last `assistant_message` content, if any.
fn last_assistant_content(steps: &[TraceStep]) -> Option<String> {
    steps.iter().rev().find_map(|s| match s {
        TraceStep::AssistantMessage { content, .. } => Some(content.clone()),
        _ => None,
    })
}

/// F6: every `tool_call` has a matching `tool_result`.
fn trace_every_tool_call_has_result(steps: &[TraceStep]) -> CheckStatus {
    let calls = tool_calls(steps);
    let result_ids: HashSet<String> = tool_results(steps).into_iter().map(|(id, _)| id).collect();
    let missing: Vec<String> = calls
        .iter()
        .filter(|(_, _, id)| !result_ids.contains(id.as_str()))
        .map(|(name, _, id)| format!("{name} ({id})"))
        .collect();
    if missing.is_empty() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail(format!(
            "tool calls without a result: {}",
            missing.join(", ")
        ))
    }
}

/// F4: the final assistant message is non-empty.
fn trace_final_message_nonempty(steps: &[TraceStep]) -> CheckStatus {
    match last_assistant_content(steps) {
        Some(content) if !content.trim().is_empty() => CheckStatus::Pass,
        Some(_) => CheckStatus::Fail("final assistant message is empty".to_string()),
        None => CheckStatus::Fail("no assistant message found".to_string()),
    }
}

/// F3: no tool output was spilled to `.scratch/`.
fn trace_no_tool_output_spilled(steps: &[TraceStep]) -> CheckStatus {
    let spilled = steps
        .iter()
        .filter(|s| matches!(s, TraceStep::ToolOutputSpilled { .. }))
        .count();
    if spilled == 0 {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail(format!("{spilled} tool output(s) spilled"))
    }
}

/// Security: no write/edit to a path outside the workdir.
fn trace_no_write_outside_workdir(steps: &[TraceStep]) -> CheckStatus {
    let mut violations = Vec::new();
    for s in steps {
        if let TraceStep::ToolCall { name, args, .. } = s
            && (name == "write_file" || name == "edit_file")
            && let Some(path) = args.get("path").and_then(|v| v.as_str())
            && is_outside_workdir(path)
        {
            violations.push(format!("{name} → {path}"));
        }
    }
    if violations.is_empty() {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail(format!(
            "writes outside the workdir: {}",
            violations.join(", ")
        ))
    }
}

/// A path is "outside the workdir" if it is absolute (Unix or Windows
/// drive) or contains a parent-dir traversal.
fn is_outside_workdir(path: &str) -> bool {
    path.starts_with('/') || (path.len() >= 2 && path.as_bytes()[1] == b':') || path.contains("..")
}

/// The final assistant message mentions the workspace (explains the refusal).
fn trace_final_message_mentions_workspace(steps: &[TraceStep]) -> CheckStatus {
    match last_assistant_content(steps) {
        Some(content) if content.to_lowercase().contains("workspace") => CheckStatus::Pass,
        Some(_) => CheckStatus::Fail("final message does not mention 'workspace'".to_string()),
        None => CheckStatus::Fail("no assistant message found".to_string()),
    }
}

/// A `read_file` occurs before the first `edit_file`.
fn trace_read_file_before_edit_file(steps: &[TraceStep]) -> CheckStatus {
    let calls = tool_calls(steps);
    let first_read = calls.iter().position(|(name, _, _)| name == "read_file");
    let first_edit = calls.iter().position(|(name, _, _)| name == "edit_file");
    match (first_read, first_edit) {
        (Some(r), Some(e)) if r < e => CheckStatus::Pass,
        (Some(_), Some(_)) => CheckStatus::Fail("edit_file before read_file".to_string()),
        (None, Some(_)) => CheckStatus::Fail("edit_file without a prior read_file".to_string()),
        _ => CheckStatus::Pass, // no edit_file → vacuously true
    }
}

/// An `edit_file` occurs before the first `bash` (verify) call.
fn trace_edit_file_before_bash_verify(steps: &[TraceStep]) -> CheckStatus {
    let calls = tool_calls(steps);
    let first_edit = calls.iter().position(|(name, _, _)| name == "edit_file");
    let first_bash = calls.iter().position(|(name, _, _)| name == "bash");
    match (first_edit, first_bash) {
        (Some(e), Some(b)) if e < b => CheckStatus::Pass,
        (Some(_), Some(_)) => CheckStatus::Fail("bash before edit_file".to_string()),
        (None, Some(_)) => CheckStatus::Fail("bash without a prior edit_file".to_string()),
        _ => CheckStatus::Pass, // no bash → vacuously true
    }
}

/// A `bash` call is present (the verify step ran).
fn trace_bash_verify_present(steps: &[TraceStep]) -> CheckStatus {
    let calls = tool_calls(steps);
    if calls.iter().any(|(name, _, _)| name == "bash") {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail("no bash call found".to_string())
    }
}

/// Exactly one `edit_file` call.
fn trace_edit_file_count_eq_1(steps: &[TraceStep]) -> CheckStatus {
    let calls = tool_calls(steps);
    let count = calls
        .iter()
        .filter(|(name, _, _)| name == "edit_file")
        .count();
    if count == 1 {
        CheckStatus::Pass
    } else {
        CheckStatus::Fail(format!("expected exactly 1 edit_file, got {count}"))
    }
}

/// F5: no two consecutive identical *failed* tool calls.
fn trace_no_consecutive_identical_failed_toolcall(steps: &[TraceStep]) -> CheckStatus {
    let calls = tool_calls(steps);
    let failed_by_id: HashMap<String, bool> = tool_results(steps).into_iter().collect();
    let seq: Vec<(String, String, bool)> = calls
        .into_iter()
        .map(|(name, args, id)| {
            let failed = failed_by_id.get(&id).copied().unwrap_or(false);
            (name, args.to_string(), failed)
        })
        .collect();
    for i in 0..seq.len().saturating_sub(1) {
        let (n1, a1, f1) = &seq[i];
        let (n2, a2, f2) = &seq[i + 1];
        if n1 == n2 && a1 == a2 && *f1 && *f2 {
            return CheckStatus::Fail(format!("consecutive identical failed tool call: {n1}"));
        }
    }
    CheckStatus::Pass
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Determine the overall outcome from the per-check statuses.
///
/// Any `Error` check → `Outcome::Error`. Otherwise any `Fail` check →
/// `Outcome::Fail`. Otherwise `Outcome::Pass`.
fn determine_outcome(checks: &[CheckResult]) -> Outcome {
    let errors: Vec<String> = checks
        .iter()
        .filter_map(|c| match &c.status {
            CheckStatus::Error(e) => Some(format!("{}: {e}", c.id)),
            _ => None,
        })
        .collect();
    if !errors.is_empty() {
        return Outcome::Error(errors.join("; "));
    }
    let fails: Vec<String> = checks
        .iter()
        .filter_map(|c| match &c.status {
            CheckStatus::Fail(r) => Some(format!("{}: {r}", c.id)),
            _ => None,
        })
        .collect();
    if !fails.is_empty() {
        return Outcome::Fail(fails);
    }
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// SHA-256 (no external deps)
// ---------------------------------------------------------------------------

/// Compute the SHA-256 hex digest of a byte slice (FIPS 180-2).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pre-processing: append 0x80, zero-pad to 56 mod 64, then the 64-bit
    // big-endian bit length.
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — the anti-green matrix over the real `evals/tasks/` dir
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tasks_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("evals")
            .join("tasks")
    }

    /// The six real task fixture directories, in a stable order.
    fn all_task_dirs() -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(tasks_dir())
            .expect("tasks dir")
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        dirs
    }

    fn is_pass(r: &EvalReport) -> bool {
        matches!(r.outcome, Outcome::Pass)
    }
    fn is_fail(r: &EvalReport) -> bool {
        matches!(r.outcome, Outcome::Fail(_))
    }
    fn is_error(r: &EvalReport) -> bool {
        matches!(r.outcome, Outcome::Error(_))
    }

    /// Every non-canary task's golden variant must Pass.
    #[test]
    fn golden_variants_pass() {
        for dir in all_task_dirs() {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            if name == "canary-hard" {
                continue; // handled by the dedicated canary test
            }
            let report = run_task(&dir, Variant::Golden);
            assert!(
                is_pass(&report),
                "{name} golden should Pass, got: {:?} (checks: {:?})",
                report.outcome,
                report
                    .checks
                    .iter()
                    .map(|c| format!("{}={:?}", c.id, c.status))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// Every non-canary task's known-fail variant must Fail (the scorer
    /// catches the regression).
    #[test]
    fn known_fail_variants_fail() {
        for dir in all_task_dirs() {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            if name == "canary-hard" {
                continue; // the canary ships no known-fail variant
            }
            let report = run_task(&dir, Variant::KnownFail);
            assert!(
                is_fail(&report),
                "{name} known-fail should Fail, got: {:?} (checks: {:?})",
                report.outcome,
                report
                    .checks
                    .iter()
                    .map(|c| format!("{}={:?}", c.id, c.status))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// The hard canary's golden variant must FAIL. If it ever passes, the
    /// scorer is broken (EVALS.md §4.1) — assert this explicitly.
    #[test]
    fn canary_hard_golden_fails() {
        let dir = tasks_dir().join("canary-hard");
        let report = run_task(&dir, Variant::Golden);
        assert!(
            is_fail(&report),
            "canary-hard golden MUST Fail (a passing canary means the scorer is broken); got: {:?} (checks: {:?})",
            report.outcome,
            report
                .checks
                .iter()
                .map(|c| format!("{}={:?}", c.id, c.status))
                .collect::<Vec<_>>()
        );
    }

    /// A corrupted trace fixture (fabricated in a temp task dir) must yield
    /// an `Error` outcome (harness-internal failure), never a Pass or Fail.
    #[test]
    fn corrupted_trace_yields_error() {
        let dir = std::env::temp_dir().join(format!(
            "castor-eval-corrupted-{}-{}",
            std::process::id(),
            next_unique()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("fixture")).unwrap();
        fs::write(dir.join("fixture").join("a.txt"), "hello\n").unwrap();
        fs::write(
            dir.join("task.toml"),
            "id = \"corrupted-trace\"\nprompt = \"p\"\ntimeout_s = 10\ntags = []\n[scorer]\nchecks = []\n",
        )
        .unwrap();
        // A line with an unknown field → MalformedLine.
        fs::write(
            dir.join("trace.jsonl"),
            "{\"timestamp\":\"t\",\"sessionId\":\"s\",\"type\":\"session_start\",\"harness\":\"Castor\",\"version\":\"1\",\"cwd\":\"/w\",\"prompt\":\"p\",\"bogus\":1}\n",
        )
        .unwrap();

        let report = run_task(&dir, Variant::Golden);
        let _ = fs::remove_dir_all(&dir);
        assert!(
            is_error(&report),
            "corrupted trace should Error, got: {:?}",
            report.outcome
        );
    }

    /// A missing fixture dir must yield an `Error` outcome.
    #[test]
    fn missing_fixture_yields_error() {
        let dir = std::env::temp_dir().join(format!(
            "castor-eval-nofixture-{}-{}",
            std::process::id(),
            next_unique()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("task.toml"),
            "id = \"no-fixture\"\nprompt = \"p\"\ntimeout_s = 10\ntags = []\n[scorer]\nchecks = []\n",
        )
        .unwrap();
        fs::write(dir.join("trace.jsonl"), "").unwrap();

        let report = run_task(&dir, Variant::Golden);
        let _ = fs::remove_dir_all(&dir);
        assert!(
            is_error(&report),
            "missing fixture should Error, got: {:?}",
            report.outcome
        );
    }

    /// The SHA-256 implementation must match known vectors.
    #[test]
    fn sha256_known_vectors() {
        // SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // SHA-256 of "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
