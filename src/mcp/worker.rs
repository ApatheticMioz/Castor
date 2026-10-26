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
//! 2. Runs the DGI gatekeeper classification; a rejected dispatch fails
//!    fast before any lease is held.
//! 3. Acquires a token-budgeted execution slot from [`TaskSemaphore`].
//! 4. Transitions the task status to `Executing` in [`TaskRegistry`] and records its PID.
//! 5. Sets up the session [`EventLogger`], indexes relevant skills, and prepares [`CompositeExecutor`].
//! 6. Drives [`crate::runner::run_session`].
//! 7. Mirrors terminal status (`Completed` or `Failed`) to [`TaskRegistry`].
//! 8. Releases the slot lease on both success and failure.

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

/// Build the worker system prompt in strict low-churn → high-churn KV prefix
/// order so the Turn-1 prefix is byte-for-byte stable across sessions in the
/// same workspace:
///   (a) Static persona, invariants, and tool-routing hierarchy (most stable)
///   (b) `[Workspace]` root, then the deterministic `[Workspace Skeleton]`
///   (c) The active-skills index (highest churn — skills can vary per dispatch)
///
/// Extracted from [`run_job_with_engine`] as a pure function of its inputs so
/// the prefix ordering can be regression-tested without spawning a job.
pub fn build_worker_system_prompt(
    workspace_root: &std::path::Path,
    skeleton_enabled: bool,
    skeleton: &str,
    skills_index: &str,
) -> String {
    let mut system_prompt = String::from(
        "You are Castor, an autonomous senior peer engineer pairing with the Lead Architect.\n\n\
Peer Engineering Invariants:\n\
- Senior Peer Mandate: Reason, decide, explore, and execute autonomously. Challenge flawed assumptions, cite conflicting code coordinates, and propose cleaner architectural alternatives rather than looping on broken premises.\n\
- Ground-Truth Hierarchy: Active source code and compiler/test diagnostics are ground truth. Documentation and notes are reference ledgers. Write present-state truth only—never historical changelogs, ticket tags, or defensive lore.\n\
- Tool Routing Hierarchy (Prefer Native Tools Over Bash):\n\
  * Code Discovery & Refactoring: Use `ast_search` and `ast_replace` for structural syntax queries and multi-file rewrites; use `search_code`, `list_dir`, `read_file`, `edit_file`, and `write_file` for file operations.\n\
  * Version Control: Use `git_status`, `git_diff`, `git_log`, and `git_commit` instead of spawning `bash` for local git operations.\n\
  * Data Inspection & Calculation: Use `python_repl` (persistent in-session namespace) instead of repeated `bash python -c` subshells.\n\
  * Visual & Document Inspection: Use `read_file` directly on local images (`.png`, `.jpg`, `.webp`, `.gif`, `.bmp`) and PDFs.\n\
  * Research: Use `paper_lookup` (OpenAlex), `web_search`, and `web_fetch`.\n\
  * Shell (`bash`): Reserve strictly for builds (`cargo build`), test/lint gates (`cargo test`, `cargo clippy`), and process/CI execution.\n\
- Verification Discipline: Never claim completion or close a task without running the verification gate and confirming clean output.",
    );

    system_prompt.push_str(&format!(
        "\n\n[Workspace]\nRoot: {}",
        workspace_root.display()
    ));
    if skeleton_enabled && !skeleton.is_empty() {
        system_prompt.push('\n');
        system_prompt.push_str(skeleton);
    }
    if !skills_index.is_empty() {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(skills_index);
    }
    system_prompt
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
    let max_ctx = config.max_context.unwrap_or(0);
    let sem = TaskSemaphore::new(state, max_slots).with_token_budget(max_ctx, max_ctx / 6);

    // 1. DGI gatekeeper: classify the dispatch BEFORE committing to a slot
    //    lease. A rejected dispatch never holds a lease (no release
    //    bookkeeping needed) and cannot block other waiters in the FIFO queue.
    if config.base_url.is_some() && config.model.is_some() {
        let dgi = if crate::mcp::dgi::warm_session_bypass(config, state, &spec.session_id) {
            tracing::debug!(
                session_id = %spec.session_id,
                "[dgi] warm-session bypass: admitting without probe"
            );
            crate::mcp::dgi::DgiVerdict::Admit
        } else {
            let lc = crate::engine::EngineLifecycle::new(config, state);
            if !lc.canary().await {
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
                match crate::mcp::dgi::evaluate_configured_model_probe(config, &spec.prompt, &http)
                    .await
                {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "[dgi] warning: dispatch classification probe failed ({err}). \
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
            return Err(reject_msg);
        }
    }

    // 2. Acquire execution slot (token-budgeted).
    // Include any resumed-history tokens so the slot's initial budget
    // reflects the true working set (history + new prompt).
    let resumed_tokens = runner::resumed_history_tokens(state, &spec.session_id, Some(max_ctx));
    let initial_tokens = ((spec.prompt.chars().count() / 4) as u32).saturating_add(resumed_tokens);
    let lease = sem.acquire_with_tokens(&spec.task_id, initial_tokens).await;
    let shared_lease = std::sync::Arc::new(std::sync::Mutex::new(lease));

    // 3. Transition status to Executing and stamp worker PID.
    let now = now_epoch_ms();
    let _ = registry
        .update(&spec.task_id, |r| {
            r.status = TaskStatus::Executing;
            r.pid = Some(std::process::id());
            r.started_at = Some(now);
            r.heartbeat = now;
        })
        .await;

    // 4. Initialize EventLogger and record session start.
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

    // 5. Discover and index skills.
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

    // 6. Build system prompt in strict low-churn -> high-churn KV prefix order
    //    (delegated to the pure [`build_worker_system_prompt`] helper so the
    //    ordering is unit-testable).
    let skeleton = if config.workspace_skeleton_prefix {
        // A byte-identical, deterministic `[Workspace Skeleton]` block (a pure
        // function of the workspace's stable top-level shape) — a warm KV prefix
        // so 96%+ of the Turn-1 prefix cache hits across concurrent sessions.
        runner::build_workspace_skeleton(std::path::Path::new(&spec.cwd))
    } else {
        String::new()
    };
    let system_prompt = build_worker_system_prompt(
        &workspace_root,
        config.workspace_skeleton_prefix,
        &skeleton,
        &skills_index,
    );

    // 7. Initialize optional extensions and composite tool executor.
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
            let guard = shared_lease.lock().unwrap();
            let _ = sem.release(&guard);
            return Err(err_msg);
        }
    };

    let tools = executor.tool_schemas();
    let turns_budget = spec.turns_budget.unwrap_or(runner::DEFAULT_TURNS_BUDGET);

    // Terminal artifacts (the reasoning-ceiling salvage report) are persisted
    // under the state dir's `.scratch/`. The per-session reasoning-effort
    // tier (validated at dispatch) is threaded into the runner so every
    // engine call in the session carries it.
    let mut options = runner::SessionOptions::with_state(state.clone())
        .with_reasoning_tags(
            config.reasoning_open_tag.clone(),
            config.reasoning_close_tag.clone(),
        )
        .with_turn_watchdog(config.turn_timeout_secs, config.max_turn_reasoning_tokens);
    options.workspace = Some(workspace_root.clone());
    options.reasoning_effort = spec.reasoning_effort.clone();
    options.probe_budget = config.probe_budget;
    // 0 when unset → the session-history watermark is disabled (zero behavior
    // change for callers that don't configure a context budget).
    options.max_context = max_ctx as usize;
    // Thread the operator-configured spill ceiling so oversized tool outputs
    // are spooled to the scratch dir (keeping the in-conversation footprint
    // bounded and the Turn-1 prefix stable).
    options.tool_spill_bytes = Some(config.tool_spill_bytes);
    options = options.with_lease_context(runner::LeaseYieldContext {
        sem: std::sync::Arc::new(sem.clone()),
        lease: shared_lease.clone(),
        task_id: spec.task_id.clone(),
        min_eviction_detect_tokens: config.min_eviction_detect_tokens,
        cache_hit_yield_ratio: config.cache_hit_yield_ratio,
    });

    // 8. Run session loop.
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

    // 9. Transition terminal status and release slot.
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
            let guard = shared_lease.lock().unwrap();
            let _ = sem.release(&guard);
            Ok(session_res.final_text)
        }
        Err(e) => {
            let err_msg = e.to_string();
            let _ = registry
                .transition(&spec.task_id, TaskStatus::Failed, Some(err_msg.clone()))
                .await;
            let guard = shared_lease.lock().unwrap();
            let _ = sem.release(&guard);
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
                reasoning_content: None,
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
                reasoning_content: None,
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
        config.base_url = None;

        let res = run_job(&spec, &state, &config).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("Engine auto-boot failed"));

        let rec = registry.get(&task_id).await.expect("task record exists");
        assert_eq!(rec.status, TaskStatus::Failed);
        assert!(rec.reason.unwrap().contains("Engine auto-boot failed"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression guard for the KV-prefix ordering invariant: the assembled
    /// worker system prompt MUST place the static persona at byte 0, then the
    /// `[Workspace]` root block, then the deterministic `[Workspace Skeleton]`,
    /// and finally the active-skills index (lowest → highest churn). This is
    /// what keeps the Turn-1 prefix byte-for-byte stable across sessions so the
    /// KV cache hits. Any reordering here silently degrades prefix reuse.
    #[test]
    fn system_prompt_prefix_ordering_is_stable_and_ordered() {
        let root = std::path::PathBuf::from("/work/repo");
        let skeleton = "[Workspace Skeleton]\n- src/\n- Cargo.toml\n- CLAUDE.md\n";

        let skills = vec![
            crate::skills::Skill {
                name: "canary-test-staging".into(),
                description: "Stage changes behind a targeted test file".into(),
                path: std::path::PathBuf::from("/work/repo/skills/canary-test-staging/SKILL.md"),
            },
            crate::skills::Skill {
                name: "hypothesis-generation".into(),
                description: "Structured root-cause hypothesis generation".into(),
                path: std::path::PathBuf::from("/work/repo/skills/hypothesis-generation/SKILL.md"),
            },
        ];
        let skills_index = skills::render_index(&skills);
        let prompt = build_worker_system_prompt(&root, true, skeleton, &skills_index);

        // (a) The static persona is byte-0 and is the FIRST text in the prompt.
        let persona = "You are Castor, an autonomous senior peer engineer";
        assert!(
            prompt.starts_with(persona),
            "persona must be byte 0, got prefix: {prompt:?}"
        );

        // Every section must appear, and each strictly after the last (ordering).
        let workspace = "[Workspace]\nRoot: /work/repo";
        let skeleton_marker = "[Workspace Skeleton]";
        // The skills index block is the `render_index` header (present-state
        // truth is a lowercase, unbracketed "Available skills." line).
        let skills_marker = "Available skills.";

        let p_persona = prompt.find(persona).expect("persona present");
        let p_workspace = prompt.find(workspace).expect("[Workspace] block present");
        let p_skeleton = prompt
            .find(skeleton_marker)
            .expect("[Workspace Skeleton] present");
        let p_skills = prompt.find(skills_marker).expect("skills index present");

        assert_eq!(p_persona, 0, "persona must start at byte 0");
        assert!(
            p_persona < p_workspace,
            "persona must precede [Workspace] (got persona={p_persona}, workspace={p_workspace})"
        );
        assert!(
            p_workspace < p_skeleton,
            "[Workspace] must precede [Workspace Skeleton] (got workspace={p_workspace}, skeleton={p_skeleton})"
        );
        assert!(
            p_skeleton < p_skills,
            "[Workspace Skeleton] must precede the skills index (got skeleton={p_skeleton}, skills={p_skills})"
        );

        // The `[Workspace]` block carries the exact root, and the skeleton
        // header is immediately followed by its deterministic entries.
        assert!(
            prompt.contains("\n\n[Workspace]\nRoot: /work/repo\n[Workspace Skeleton]\n- src/\n- Cargo.toml\n- CLAUDE.md\n"),
            "root and skeleton must be contiguous with a single newline join"
        );

        // The skills index is the terminal block (after the skeleton).
        let skills_suffix = &prompt[p_skills..];
        assert!(
            skills_suffix.contains("- canary-test-staging:")
                && skills_suffix.contains("- hypothesis-generation:"),
            "skills index must list every active skill: {skills_suffix}"
        );
    }

    /// When the workspace-skeleton prefix is disabled, the prompt still opens
    /// with the persona at byte 0, keeps the `[Workspace]` root, and omits the
    /// skeleton block entirely (but still appends the skills index).
    #[test]
    fn system_prompt_without_skeleton_still_ordered() {
        let root = std::path::PathBuf::from("/work/repo");
        let skills_index = skills::render_index(&[crate::skills::Skill {
            name: "hypothesis-generation".into(),
            description: "Structured root-cause hypothesis generation".into(),
            path: std::path::PathBuf::from("/work/repo/skills/hypothesis-generation/SKILL.md"),
        }]);

        // skeleton_enabled=false: even a non-empty skeleton string is ignored.
        let prompt = build_worker_system_prompt(
            &root,
            false,
            "[Workspace Skeleton]\n- src/\n",
            &skills_index,
        );

        assert!(prompt.starts_with("You are Castor, an autonomous senior peer engineer"));
        let p_workspace = prompt
            .find("[Workspace]\nRoot: /work/repo")
            .expect("workspace present");
        let p_skills = prompt
            .find("Available skills.")
            .expect("skills index present");
        assert!(
            p_workspace < p_skills,
            "[Workspace] must precede the skills index when skeleton is disabled"
        );
        assert!(
            !prompt.contains("[Workspace Skeleton]"),
            "skeleton must be omitted when disabled"
        );
    }
}
