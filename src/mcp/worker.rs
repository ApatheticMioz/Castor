//! Background worker process implementation for Castor coworker tasks.
//!
//! When a task is dispatched, the MCP server spawns a detached worker process:
//!
//! ```bash
//! castor __worker <path-to-job-spec.json>
//! ```
//!
//! The worker:
//! 1. Loads the [`JobSpec`].
//! 2. Acquires an execution slot from [`TaskSemaphore`].
//! 3. Transitions the task status to `Executing` in [`TaskRegistry`] and records its PID.
//! 4. Sets up the session [`EventLogger`], indexes relevant skills, and prepares [`CompositeExecutor`].
//! 5. Drives [`crate::runner::run_session`].
//! 6. Mirrors terminal status (`Completed` or `Failed`) to [`TaskRegistry`].
//! 7. Releases the slot lease on both success and failure.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Config;
use crate::engine::EngineClient;
use crate::runner::events::EventLogger;
use crate::runner::{self, ChatEngine};
use crate::skills;
use crate::state::StateDir;
use crate::task::registry::{TaskRegistry, TaskStatus};
use crate::task::semaphore::TaskSemaphore;
use crate::tools::CompositeExecutor;
use crate::tools::extensions::ExtensionBridge;

/// A serialized job specification written by the MCP spawner for the detached worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub task_id: String,
    pub prompt: String,
    pub cwd: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Canonicalize the workspace root by walking up from the provided path
/// to find the repository boundary (.git, Cargo.toml, package.json).
/// Falls back to the provided path if no root marker is found.
pub fn find_workspace_root(p: &std::path::Path) -> std::path::PathBuf {
    let mut cur = if p.is_file() {
        p.parent().unwrap_or(p).to_path_buf()
    } else {
        p.to_path_buf()
    };
    loop {
        if cur.join(".git").exists()
            || cur.join("Cargo.toml").exists()
            || cur.join("package.json").exists()
        {
            return cur;
        }
        if let Some(parent) = cur.parent() {
            if parent == cur {
                break;
            }
            cur = parent.to_path_buf();
        } else {
            break;
        }
    }
    p.to_path_buf()
}

/// Execute a job specification against the real local engine.
pub async fn run_job(spec: &JobSpec, state: &StateDir, config: &Config) -> Result<String, String> {
    let registry = TaskRegistry::new(state);
    let now = now_epoch_ms();
    let _ = registry
        .update(&spec.task_id, |r| {
            r.pid = Some(std::process::id());
            r.heartbeat = now;
        })
        .await;

    let lc = crate::engine::EngineLifecycle::new(config, state);
    if let Err(e) = lc.ensure_running().await {
        let err_msg = format!("Engine auto-boot failed: {e}");
        let _ = registry
            .transition(&spec.task_id, TaskStatus::Failed, Some(err_msg.clone()))
            .await;
        return Err(err_msg);
    }
    let engine = match EngineClient::from_config(config) {
        Ok(eng) => eng,
        Err(e) => {
            let err_msg = e.to_string();
            let _ = registry
                .transition(&spec.task_id, TaskStatus::Failed, Some(err_msg.clone()))
                .await;
            return Err(err_msg);
        }
    };
    run_job_with_engine(spec, state, config, &engine).await
}

/// Execute a job specification with a provided [`ChatEngine`] (supports mock engines in tests).
pub async fn run_job_with_engine(
    spec: &JobSpec,
    state: &StateDir,
    config: &Config,
    engine: &dyn ChatEngine,
) -> Result<String, String> {
    let registry = TaskRegistry::new(state);
    let max_slots = config.max_concurrent_tasks as usize;
    let sem = TaskSemaphore::new(state, max_slots);

    // 1. Acquire execution slot.
    let lease = sem.acquire(&spec.task_id).await;

    // 2. Transition status to Executing and stamp worker PID.
    let now = now_epoch_ms();
    let _ = registry
        .update(&spec.task_id, |r| {
            r.status = TaskStatus::Executing;
            r.pid = Some(std::process::id());
            r.started_at = Some(now);
            r.heartbeat = now;
        })
        .await;

    // 2.5 Phase 1: DGI Gatekeeper (model-driven 1-forward pass logit probe via guided_choice).
    if let (Some(base_url), Some(model)) = (&config.base_url, &config.model) {
        let lc = crate::engine::EngineLifecycle::new(config, state);
        let dgi = if !lc.canary().await {
            crate::mcp::dgi::evaluate(&spec.prompt)
        } else {
            let probe_timeout_secs = std::env::var("CASTOR_DGI_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30);
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(probe_timeout_secs))
                .build()
                .unwrap_or_default();
            match crate::mcp::dgi::evaluate_model_probe(base_url, model, &spec.prompt, &http).await {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "[dgi] warning: 1-forward pass model probe failed ({err}). \
                         Falling back to soft heuristic."
                    );
                    let fallback = crate::mcp::dgi::evaluate(&spec.prompt);
                    if matches!(fallback, crate::mcp::dgi::DgiVerdict::Admit) {
                        crate::mcp::dgi::DgiVerdict::Review(0)
                    } else {
                        fallback
                    }
                }
            }
        };

        if let crate::mcp::dgi::DgiVerdict::Reject(sigs) = &dgi {
            let body = sigs
                .iter()
                .map(|s| format!("  - {s}"))
                .collect::<Vec<_>>()
                .join("\n");
            let reject_msg = format!(
                "DecompositionGateRejected: dispatch rejected by model decomposition gatekeeper:\n{body}\n\
                 Decompose into a single-concern slice (one subsystem, one verification gate) and re-dispatch."
            );
            let _ = registry
                .transition(&spec.task_id, TaskStatus::Failed, Some(reject_msg.clone()))
                .await;
            let _ = sem.release(&lease);
            return Err(reject_msg);
        }
    }

    // 3. Initialize EventLogger and record session start.
    let logger = EventLogger::new(state, &spec.session_id);
    // The effective tier is recorded on session_start (null when unset) so
    // the session ledger is self-explanatory: null = "server default applied".
    let _ = logger.append(json!({
        "type": "session_start",
        "sessionId": spec.session_id,
        "taskId": spec.task_id,
        "cwd": spec.cwd,
        "prompt": spec.prompt,
        "reasoning_effort": spec.reasoning_effort,
    }));

    let host_raw_path = crate::platform::to_host_path(&spec.cwd);
    let workspace_root = find_workspace_root(&host_raw_path);

    // 4. Discover and index skills.
    let skill_dirs = [
        workspace_root.join(".agents").join("skills"),
        workspace_root.join("skills"),
        state.root().join("skills"),
    ];
    let all_skills = skills::load_skills(&skill_dirs);
    let active_skills: Vec<_> = if let Some(ref req) = spec.skills {
        all_skills
            .into_iter()
            .filter(|s| req.contains(&s.name))
            .collect()
    } else {
        all_skills
    };
    let skills_index = skills::render_index(&active_skills);

    // 5. Build system prompt.
    let system_prompt = format!(
        "You are Castor, an autonomous senior peer programmer. Working directory: {}\n\n\
Peer Programming Invariants:\n\
- Senior Peer Mandate: You are an autonomous senior peer engineer pairing with the Lead Architect, \
not a passive execution tool. You are expected to reason, decide, and act on your own behalf.\n\
- Proactive Pushback & Impasse Transparency: If a dispatch presents contradictory requirements, \
impossible constraints, or flawed assumptions, do NOT enter silent trial-and-error loops or mutate \
unverified code. Proactively state the trade-offs, cite the conflicting coordinates, and propose the \
cleaner architectural alternative.\n\
- Ground-Truth Hierarchy: Active code and compiler diagnostics are ground truth; historical audit \
notes or deleted legacy references are reference ledgers.\n\
- Verification Discipline: Never mask unverified mutations; verify against active test gates.\n\n{}",
        workspace_root.display(),
        skills_index
    );

    // 6. Initialize optional extensions and composite tool executor.
    let ext_bridge = if let Some(ref exts) = spec.extensions {
        ExtensionBridge::spawn(exts).await.ok()
    } else {
        None
    };

    let executor = match CompositeExecutor::with_config(
        &workspace_root,
        config.searxng_url.clone(),
        config.brave_api_key.clone(),
        config.openalex_email.clone(),
        config.openalex_api_key.clone(),
        ext_bridge,
    ) {
        Ok(exec) => exec,
        Err(e) => {
            let err_msg = format!("failed to initialize executor: {e}");
            let _ = registry
                .transition(&spec.task_id, TaskStatus::Failed, Some(err_msg.clone()))
                .await;
            let _ = sem.release(&lease);
            return Err(err_msg);
        }
    };

    let tools = executor.tool_schemas();
    let turns_budget = spec.turns_budget.unwrap_or(runner::DEFAULT_TURNS_BUDGET);

    // Terminal artifacts (the reasoning-ceiling salvage report) are persisted
    // under the state dir's `.scratch/`. The per-session reasoning-effort
    // tier (validated at dispatch) is threaded into the runner so every
    // engine call in the session carries it.
    let mut options = runner::SessionOptions::with_state(state.clone());
    options.reasoning_effort = spec.reasoning_effort.clone();
    options.probe_budget = config.probe_budget;

    // 7. Run session loop.
    let outcome = runner::run_session(
        engine,
        &executor,
        &logger,
        &system_prompt,
        &spec.prompt,
        &tools,
        turns_budget,
        Some(&options),
    )
    .await;

    // 8. Transition terminal status and release slot.
    match outcome {
        Ok(session_res) => {
            let terminal_status = if session_res.status == "failed" {
                TaskStatus::Failed
            } else {
                TaskStatus::Completed
            };
            let _ = registry
                .transition(
                    &spec.task_id,
                    terminal_status,
                    Some(session_res.final_text.clone()),
                )
                .await;
            let _ = sem.release(&lease);
            Ok(session_res.final_text)
        }
        Err(e) => {
            let err_msg = e.to_string();
            let _ = registry
                .transition(&spec.task_id, TaskStatus::Failed, Some(err_msg.clone()))
                .await;
            let _ = sem.release(&lease);
            Err(err_msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Completion, EngineError, Message, Metrics, ToolSchema};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    struct ScriptedEngine {
        responses: tokio::sync::Mutex<VecDeque<Completion>>,
    }

    #[async_trait]
    impl ChatEngine for ScriptedEngine {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSchema],
            _stream: bool,
            _reasoning_effort: Option<&str>,
        ) -> Result<Completion, EngineError> {
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| EngineError::Malformed("no scripted response".into()))
        }
    }

    fn tmp_state() -> (StateDir, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "castor_worker_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = StateDir::new(&dir);
        let _ = state.ensure();
        (state, dir)
    }

    #[tokio::test]
    async fn run_job_happy_path_transitions_to_completed() {
        let (state, dir) = tmp_state();
        let registry = TaskRegistry::new(&state);

        let task_id = registry
            .create("write a test file", dir.to_str().unwrap(), "sess_1")
            .await
            .unwrap();

        let spec = JobSpec {
            task_id: task_id.clone(),
            prompt: "write a test file".to_string(),
            cwd: dir.to_str().unwrap().to_string(),
            session_id: "sess_1".to_string(),
            reasoning_effort: None,
            extensions: None,
            skills: None,
            test_command: None,
            turns_budget: Some(10),
            timeout_ms: None,
        };

        let engine = ScriptedEngine {
            responses: tokio::sync::Mutex::new(VecDeque::from(vec![Completion {
                content: "Task successfully finished.".to_string(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".to_string()),
                metrics: Metrics {
                    ttft_ms: None,
                    total_ms: 10.0,
                    tokens_per_sec: None,
                    prompt_tokens: None,
                    cached_tokens: None,
                    completion_tokens: None,
                    reasoning_tokens: None,
                },
            }])),
        };

        let loaded = crate::config::load().expect("load default config");
        let mut config = loaded.config;
        config.state_dir = dir.clone();

        let res = run_job_with_engine(&spec, &state, &config, &engine)
            .await
            .unwrap();
        assert_eq!(res, "Task successfully finished.");

        // Verify task record is Completed.
        let rec = registry.get(&task_id).await.unwrap();
        assert_eq!(rec.status, TaskStatus::Completed);
        assert_eq!(rec.reason, Some("Task successfully finished.".to_string()));

        // Verify event log was written.
        let logger = EventLogger::new(&state, "sess_1");
        let events = logger.read_all();
        let session_start = events
            .iter()
            .find(|e| e["type"] == "session_start")
            .expect("session_start event present");
        // Unset effort: the field is present and null (honest "server default
        // applied" — not missing, which would be indistinguishable from a
        // pre-override session).
        assert_eq!(
            session_start["reasoning_effort"],
            serde_json::Value::Null,
            "session_start must carry reasoning_effort=null when unset: {session_start}"
        );
        assert!(events.iter().any(|e| e["type"] == "final"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dispatch carrying a per-session `reasoning_effort` tier records the
    /// tier on the `session_start` event and threads it into
    /// [`runner::SessionOptions`] (which the runner forwards to the engine on
    /// every chat turn).
    #[tokio::test]
    async fn session_start_carries_reasoning_effort() {
        let (state, dir) = tmp_state();
        let registry = TaskRegistry::new(&state);

        let task_id = registry
            .create("effort test", dir.to_str().unwrap(), "sess_effort")
            .await
            .unwrap();

        let spec = JobSpec {
            task_id: task_id.clone(),
            prompt: "effort test".to_string(),
            cwd: dir.to_str().unwrap().to_string(),
            session_id: "sess_effort".to_string(),
            reasoning_effort: Some("xhigh".to_string()),
            extensions: None,
            skills: None,
            test_command: None,
            turns_budget: Some(10),
            timeout_ms: None,
        };

        let engine = ScriptedEngine {
            responses: tokio::sync::Mutex::new(VecDeque::from(vec![Completion {
                content: "done at xhigh".to_string(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".to_string()),
                metrics: Metrics {
                    ttft_ms: None,
                    total_ms: 10.0,
                    tokens_per_sec: None,
                    prompt_tokens: None,
                    cached_tokens: None,
                    completion_tokens: None,
                    reasoning_tokens: None,
                },
            }])),
        };

        let loaded = crate::config::load().expect("load default config");
        let mut config = loaded.config;
        config.state_dir = dir.clone();

        let res = run_job_with_engine(&spec, &state, &config, &engine)
            .await
            .unwrap();
        assert_eq!(res, "done at xhigh");

        // The session_start event carries the tier string.
        let logger = EventLogger::new(&state, "sess_effort");
        let events = logger.read_all();
        let session_start = events
            .iter()
            .find(|e| e["type"] == "session_start")
            .expect("session_start event present");
        assert_eq!(
            session_start["reasoning_effort"],
            serde_json::json!("xhigh"),
            "session_start must carry the tier string: {session_start}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_job_marks_task_failed_when_auto_boot_fails() {
        let dir = std::env::temp_dir().join(format!("test_boot_fail_{}", now_epoch_ms()));
        let state = StateDir::new(&dir);
        state.ensure().unwrap();

        let registry = TaskRegistry::new(&state);
        let task_id = registry
            .create("failing boot task", "test", "sess_boot_fail")
            .await
            .unwrap();

        let spec = JobSpec {
            task_id: task_id.clone(),
            prompt: "failing boot task".into(),
            cwd: "test".into(),
            session_id: "sess_boot_fail".into(),
            reasoning_effort: None,
            extensions: None,
            skills: None,
            test_command: None,
            turns_budget: None,
            timeout_ms: None,
        };

        let loaded = crate::config::load().expect("load default config");
        let mut config = loaded.config;
        config.state_dir = dir.clone();
        config.launch_command = None;
        config.ports.engine = 0;

        let res = run_job(&spec, &state, &config).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("Engine auto-boot failed"));

        let rec = registry.get(&task_id).await.expect("task record exists");
        assert_eq!(rec.status, TaskStatus::Failed);
        assert!(rec.reason.unwrap().contains("Engine auto-boot failed"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
