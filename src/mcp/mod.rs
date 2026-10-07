//! MCP server implementation.
//!
//! Serves three tools over stdio: `<prefix>_coworker`, `<prefix>_task`,
//! `<prefix>_server`.
//!
//! All logging goes to stderr; stdout is reserved for JSON-RPC frames.

pub mod dgi;
pub mod worker;

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::serve_server;
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Default tool-name prefix (matches `config::Config::tool_prefix` default).
pub const DEFAULT_TOOL_PREFIX: &str = "castor";

/// Base tool names (without prefix).
pub const TOOL_COWORKER: &str = "coworker";
pub const TOOL_TASK: &str = "task";
pub const TOOL_SERVER: &str = "server";

/// The three base tool names, in canonical order.
pub const TOOL_BASE_NAMES: [&str; 3] = [TOOL_COWORKER, TOOL_TASK, TOOL_SERVER];

/// Input schema for the `coworker` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CoworkerParams {
    /// Task, inquiry, or architectural instruction (pure text-only).
    pub prompt: String,
    /// Working directory for filesystem and shell tools (defaults to current workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Named persistent session ID (maintains KV-cache and conversation context across turns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Per-dispatch reasoning-effort tier (xhigh | medium | low).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Optional stdio extensions (e.g. `["uvx free-search-mcp"]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Explicit list of skill names to inject (bypasses keyword auto-matching).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Optional verification test/benchmark command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_command: Option<String>,
    /// Task timeout in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// (Deprecated) Kept for schema/backward compatibility. The static length
    /// ceiling this once bypassed has been replaced by the DGI Gatekeeper
    /// (`mcp::dgi`), which rejects on calibrated monolith signatures rather
    /// than raw length; the field no longer gates dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_large_prompt: Option<bool>,
}

/// Input schema for the `task` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TaskParams {
    /// Task ID (required for `status`, `cancel`, `kill`, and `extend_lease`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Action to perform (status, cancel, cancel_all, list, kill, stats, extend_lease).
    pub action: String,
}

/// Input schema for the `server` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ServerParams {
    /// Lifecycle action (status, start, stop).
    pub action: String,
    /// Force stop even if a task is actively executing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Valid reasoning-effort tiers for the default serving engine (Qwen via
/// vLLM). The served template 400s on any other value (`off`, `high`,
/// `minimal`, …), so the schema fails fast here instead of letting the
/// upstream reject a dispatched session mid-run.
pub const REASONING_EFFORT_TIERS: [&str; 3] = ["xhigh", "medium", "low"];

/// Validate a per-dispatch `reasoning_effort` value against
/// [`REASONING_EFFORT_TIERS`].
///
/// `Ok(())` when the value is absent or a known tier; `Err` with a clear,
/// self-explanatory message naming the bad value and the valid set when it
/// is not.
pub fn validate_reasoning_effort(v: Option<&str>) -> Result<(), String> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    if REASONING_EFFORT_TIERS.contains(&v) {
        Ok(())
    } else {
        Err(format!(
            "Invalid reasoning_effort '{v}': expected one of [{}]. The served \
             engine template rejects other tiers with HTTP 400.",
            REASONING_EFFORT_TIERS.join(", ")
        ))
    }
}

/// The MCP server handler.
#[derive(Debug, Clone)]
pub struct CastorMcpServer {
    /// Tool-name prefix (e.g. "castor" → "castor_coworker").
    pub prefix: String,
}

impl CastorMcpServer {
    /// Create a new server with the given tool-name prefix.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    /// Build a prefixed tool name.
    pub fn tool_name(&self, base: &str) -> String {
        if self.prefix.is_empty() {
            base.to_string()
        } else {
            format!("{}_{}", self.prefix, base)
        }
    }

    /// Build the three `Tool` definitions with their JSON schemas.
    pub fn tools(&self) -> Vec<Tool> {
        let empty = std::sync::Arc::new(serde_json::Map::new());
        vec![
            Tool::new(
                self.tool_name(TOOL_COWORKER),
                "Autonomous Senior Coworker (Castor Microkernel). Primary autonomous execution \
                 coworker with full native access to Filesystem, Shell, and Git across Windows \
                 and WSL. Executes codebase exploration, refactoring, implementation, \
                 diagnostics, live web/docs research, and git operations.",
                empty.clone(),
            )
            .with_input_schema::<CoworkerParams>(),
            Tool::new(
                self.tool_name(TOOL_TASK),
                "Manage background coworker tasks: check status, retrieve output, cancel, or \
                 list tasks.",
                empty.clone(),
            )
            .with_input_schema::<TaskParams>(),
            Tool::new(
                self.tool_name(TOOL_SERVER),
                "Manage the local engine lifecycle: check status, start, or stop the \
                 local serving engine.",
                empty,
            )
            .with_input_schema::<ServerParams>(),
        ]
    }

    /// Parse and validate the `coworker` tool's JSON arguments.
    fn parse_coworker_args(args: Option<Value>) -> Result<CoworkerParams, String> {
        let Some(v) = args else {
            return Err("Missing arguments for coworker tool".into());
        };
        let params: CoworkerParams =
            serde_json::from_value(v).map_err(|e| format!("Invalid coworker arguments: {e}"))?;
        if params.prompt.trim().is_empty() {
            return Err("Error: Prompt cannot be empty.".into());
        }
        // Fail fast on a bad reasoning tier: the served template would 400
        // the whole session later, so reject at dispatch instead.
        validate_reasoning_effort(params.reasoning_effort.as_deref())
            .map_err(|e| format!("Error: {e}"))?;
        Ok(params)
    }

    /// Run the DGI Gatekeeper: 1-forward pass model probe (or soft heuristic
    /// fallback) and return the verdict together with an optional advisory
    /// note to append to the dispatch message.
    ///
    /// Queues on the 1-slot [`TaskSemaphore`] as a real task to guarantee exclusive,
    /// uncontended GPU access. Because DGI is a 1-forward pass logit probe (CIVP gate),
    /// it executes on the idle engine in <1s and immediately releases the semaphore.
    async fn evaluate_dgi(
        loaded: &crate::config::LoadedConfig,
        prompt: &str,
    ) -> (dgi::DgiVerdict, Option<String>) {
        let dgi = if let (Some(base_url), Some(model)) =
            (&loaded.config.base_url, &loaded.config.model)
        {
            let state = crate::state::StateDir::from_config(&loaded.config);
            let _ = state.ensure();
            let max_slots = loaded.config.max_concurrent_tasks as usize;
            let sem = crate::task::semaphore::TaskSemaphore::new(&state, max_slots);
            let registry = crate::task::registry::TaskRegistry::new(&state);

            let prompt_preview = if prompt.len() > 60 {
                format!("[dgi] {}...", &prompt[..60])
            } else {
                format!("[dgi] {prompt}")
            };
            let dgi_task_id = registry
                .create(&prompt_preview, "dgi", "dgi_gate")
                .await
                .unwrap_or_else(|_| format!("task_dgi_{}", now_epoch_ms()));

            // Queue on the 1-slot semaphore as a real task slot lease.
            // Guarantees exclusive engine access while executing the 1-forward pass probe.
            let lease = sem.acquire(&dgi_task_id).await;
            struct SlotGuard<'a> {
                sem: &'a crate::task::semaphore::TaskSemaphore,
                lease: Option<crate::task::semaphore::SlotLease>,
            }
            impl<'a> Drop for SlotGuard<'a> {
                fn drop(&mut self) {
                    if let Some(ref l) = self.lease.take() {
                        let _ = self.sem.release(l);
                    }
                }
            }
            let guard = SlotGuard {
                sem: &sem,
                lease: Some(lease),
            };

            let _ = registry
                .transition(&dgi_task_id, crate::task::registry::TaskStatus::Executing, None)
                .await;

            let probe_timeout_secs = std::env::var("CASTOR_DGI_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30);
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(probe_timeout_secs))
                .build()
                .unwrap_or_default();

            let probe_res = dgi::evaluate_model_probe(base_url, model, prompt, &http).await;

            // Immediately release the semaphore (<1s slot hold time).
            drop(guard);

            match probe_res {
                Ok(v) => {
                    let _ = registry
                        .transition(
                            &dgi_task_id,
                            crate::task::registry::TaskStatus::Completed,
                            Some(format!("DGI verdict: {v:?}")),
                        )
                        .await;
                    v
                }
                Err(err) => {
                    let _ = registry
                        .transition(
                            &dgi_task_id,
                            crate::task::registry::TaskStatus::Failed,
                            Some(format!("DGI probe error: {err}")),
                        )
                        .await;
                    tracing::warn!(
                        error = %err,
                        "[dgi] warning: 1-forward pass model probe failed ({err}). \
                         Engine offline, unreachable, or endpoint does not support guided_choice. \
                         Falling back to soft heuristic."
                    );
                    let fallback = dgi::evaluate(prompt);
                    if matches!(fallback, dgi::DgiVerdict::Admit) {
                        dgi::DgiVerdict::Review(0) // Special loud warning sentinel
                    } else {
                        fallback
                    }
                }
            }
        } else {
            tracing::warn!(
                "[dgi] warning: DGI running without configured engine/model; 1-forward pass probe inactive."
            );
            dgi::DgiVerdict::Review(0)
        };

        let dgi_note = match &dgi {
            dgi::DgiVerdict::Review(0) => Some(
                "- **[DGI ADVISORY]**: 1-forward pass model probe was bypassed (engine offline, unreachable, or backend unsupported). Proceeding without model-verified CIVP gate."
                    .to_string(),
            ),
            dgi::DgiVerdict::Review(s) => Some(format!(
                "- **DGI**: advisory score {s} — flag for decomposition; dispatch proceeding."
            )),
            _ => None,
        };

        (dgi, dgi_note)
    }

    /// Resolve the path to the castor worker binary, handling test-binary
    /// suffixes and debug/release fallbacks.
    fn resolve_worker_bin() -> std::path::PathBuf {
        let mut bin =
            std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("castor"));
        let bin_str = bin.to_string_lossy();
        if let Some(clean) = bin_str.strip_suffix(" (deleted)") {
            bin = std::path::PathBuf::from(clean);
        }
        if !bin.exists()
            && let Ok(cwd) = std::env::current_dir()
        {
            let debug_bin = cwd.join("target/debug/castor");
            let release_bin = cwd.join("target/release/castor");
            if debug_bin.exists() {
                bin = debug_bin;
            } else if release_bin.exists() {
                bin = release_bin;
            }
        }
        bin
    }

    async fn handle_coworker(&self, args: Option<Value>) -> CallToolResult {
        let params = match Self::parse_coworker_args(args) {
            Ok(p) => p,
            Err(e) => return CallToolResult::error(vec![ContentBlock::text(e)]),
        };

        let loaded = match crate::config::load() {
            Ok(c) => c,
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Config error: {e}"
                ))]);
            }
        };

        // DGI Gatekeeper: model-driven 1-forward pass logit probe via guided_choice.
        let (dgi, dgi_note) = Self::evaluate_dgi(&loaded, &params.prompt).await;

        if let dgi::DgiVerdict::Reject(sigs) = &dgi {
            let body = sigs
                .iter()
                .map(|s| format!("  - {s}"))
                .collect::<Vec<_>>()
                .join("\n");
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "DecompositionGateRejected: dispatch rejected by model decomposition gatekeeper:\n{body}\n\
                 Decompose into a single-concern slice (one subsystem, one verification gate) and re-dispatch.",
            ))]);
        }

        let state = crate::state::StateDir::from_config(&loaded.config);
        if let Err(e) = state.ensure() {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "Failed to create state dir: {e}"
            ))]);
        }

        let cwd = params.cwd.unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_string())
        });

        let session_id = params
            .session_id
            .unwrap_or_else(|| format!("castor_session_{}", now_epoch_ms()));

        let registry = crate::task::registry::TaskRegistry::new(&state);
        let task_id = match registry.create(&params.prompt, &cwd, &session_id).await {
            Ok(id) => id,
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Failed to register task: {e}"
                ))]);
            }
        };

        let spec = worker::JobSpec {
            task_id: task_id.clone(),
            prompt: params.prompt,
            cwd: cwd.clone(),
            session_id: session_id.clone(),
            reasoning_effort: params.reasoning_effort,
            extensions: params.extensions,
            skills: params.skills,
            test_command: params.test_command,
            turns_budget: None,
            timeout_ms: params.timeout_ms,
        };

        let spec_path = state.tasks().join(format!("job_{task_id}.json"));
        let json_str = serde_json::to_string_pretty(&spec).unwrap_or_default();
        if let Err(e) = std::fs::write(&spec_path, json_str) {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "Failed to write job spec: {e}"
            ))]);
        }

        // Spawn detached worker process.
        let bin = Self::resolve_worker_bin();
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("__worker").arg(&spec_path);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);

        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(pid) = child.id() {
                    let _ = registry
                        .update(&task_id, |r| {
                            r.pid = Some(pid);
                        })
                        .await;
                }

                // Zero-Turn Execution Contract: sync window for fast tasks (<15s).
                // Configurable via CASTOR_SYNC_TIMEOUT_SECS (default 15, 0 = immediately background).
                let sync_timeout_secs = std::env::var("CASTOR_SYNC_TIMEOUT_SECS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(15);

                if sync_timeout_secs > 0 {
                    let sync_dur = std::time::Duration::from_secs(sync_timeout_secs);
                    if let Ok(Ok(_)) = tokio::time::timeout(sync_dur, child.wait()).await {
                        let rec = registry.get(&task_id).await.or_else(|| {
                            match registry.read_disk(&task_id) {
                                crate::task::registry::DiskRead::Ok(r) => Some(r),
                                _ => None,
                            }
                        });

                        if let Some(r) = rec {
                            match r.status {
                                crate::task::registry::TaskStatus::Completed => {
                                    let out = r.reason.unwrap_or_else(|| {
                                        "Task completed successfully.".to_string()
                                    });
                                    return CallToolResult::success(vec![ContentBlock::text(out)]);
                                }
                                crate::task::registry::TaskStatus::Failed => {
                                    let err =
                                        r.reason.unwrap_or_else(|| "Task failed.".to_string());
                                    return CallToolResult::error(vec![ContentBlock::text(
                                        format!("Task failed: {err}"),
                                    )]);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let _ = registry
                    .transition(
                        &task_id,
                        crate::task::registry::TaskStatus::Failed,
                        Some(format!("Spawn failed: {e}")),
                    )
                    .await;
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Failed to spawn worker child: {e}"
                ))]);
            }
        }

        let status_port = loaded.config.ports.status;
        let (wait_cmd_win, wait_cmd_wsl) = wait_commands(status_port, &task_id);
        let status_cmd = format!(
            "{}_task(action: \"status\", task_id: \"{task_id}\")",
            self.prefix
        );

        let mut text = format!(
            "### Castor Task Dispatched (Background Execution)\n\
             - **Task ID**: `{task_id}` | **Session**: `{session_id}` | **Status**: `queued`\n\
             - **Working Directory**: `{cwd}`\n\
             > [!TIP]\n\
             > **Castor Engine Status: ACTIVELY EXECUTING**\n\
             > Task `{task_id}` dispatched to background worker.\n\
             - **Wait Command**: `{wait_cmd_win}` (WSL: `{wait_cmd_wsl}`)\n\
             - **Status Command**: `{status_cmd}`"
        );

        if let Some(note) = dgi_note {
            text.push_str(&format!("\n{note}"));
        }

        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    async fn handle_task(&self, args: Option<Value>) -> CallToolResult {
        let params: TaskParams = match args {
            Some(v) => match serde_json::from_value(v) {
                Ok(p) => p,
                Err(e) => {
                    return CallToolResult::error(vec![ContentBlock::text(format!(
                        "Invalid task arguments: {e}"
                    ))]);
                }
            },
            None => {
                return CallToolResult::error(vec![ContentBlock::text(
                    "Missing arguments for task tool",
                )]);
            }
        };

        let loaded = match crate::config::load() {
            Ok(c) => c,
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Config error: {e}"
                ))]);
            }
        };
        let state = crate::state::StateDir::from_config(&loaded.config);
        let _ = state.ensure();
        let registry = crate::task::registry::TaskRegistry::new(&state);

        match params.action.as_str() {
            "list" => {
                let tasks = registry.list().await;
                if tasks.is_empty() {
                    return CallToolResult::success(vec![ContentBlock::text("No tasks found.")]);
                }
                let mut out = String::from("ID | Status | Session | Created\n---|---|---|---\n");
                for t in tasks {
                    out.push_str(&format!(
                        "`{}` | `{}` | `{}` | {}\n",
                        t.id, t.status, t.session_id, t.created_at
                    ));
                }
                CallToolResult::success(vec![ContentBlock::text(out)])
            }
            "cancel_all" => {
                let tasks = registry.list().await;
                let mut count = 0;
                for t in tasks {
                    if !t.status.is_terminal() {
                        if let Some(pid) = t.pid {
                            crate::platform::kill_process_tree(pid);
                        }
                        let _ = registry
                            .transition(
                                &t.id,
                                crate::task::registry::TaskStatus::Cancelled,
                                Some("Cancelled by request.".into()),
                            )
                            .await;
                        count += 1;
                    }
                }
                CallToolResult::success(vec![ContentBlock::text(format!(
                    "Cancelled {count} active/queued task(s)."
                ))])
            }
            "status" => {
                let task_id = match params.task_id {
                    Some(ref id) => id,
                    None => {
                        return CallToolResult::error(vec![ContentBlock::text(
                            "Error: 'task_id' parameter is required for status.",
                        )]);
                    }
                };
                let rec = match registry.get(task_id).await {
                    Some(r) => r,
                    None => {
                        return CallToolResult::error(vec![ContentBlock::text(format!(
                            "Task '{task_id}' not found."
                        ))]);
                    }
                };
                let status_port = loaded.config.ports.status;
                let text = match rec.status {
                    crate::task::registry::TaskStatus::Completed => {
                        format!(
                            "[task] id={} status=completed\n{}",
                            rec.id,
                            rec.reason.as_deref().unwrap_or("Task completed.")
                        )
                    }
                    crate::task::registry::TaskStatus::Failed => {
                        format!(
                            "[task] id={} status=failed\nReason: {}",
                            rec.id,
                            rec.reason.as_deref().unwrap_or("Unknown failure.")
                        )
                    }
                    crate::task::registry::TaskStatus::Cancelled => {
                        format!("[task] id={} status=cancelled", rec.id)
                    }
                    crate::task::registry::TaskStatus::Queued
                    | crate::task::registry::TaskStatus::Executing => {
                        let (wait_win, wait_wsl) = wait_commands(status_port, &rec.id);
                        format!(
                            "[task] id={} status={}\nWait (Windows): {}\nWait (WSL): {}",
                            rec.id, rec.status, wait_win, wait_wsl
                        )
                    }
                };
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            "cancel" | "kill" => {
                let task_id = match params.task_id {
                    Some(ref id) => id,
                    None => {
                        return CallToolResult::error(vec![ContentBlock::text(
                            "Error: 'task_id' parameter is required for cancel.",
                        )]);
                    }
                };
                let rec = match registry.get(task_id).await {
                    Some(r) => r,
                    None => {
                        return CallToolResult::error(vec![ContentBlock::text(format!(
                            "Task '{task_id}' not found."
                        ))]);
                    }
                };
                if let Some(pid) = rec.pid {
                    crate::platform::kill_process_tree(pid);
                }
                let _ = registry
                    .transition(
                        task_id,
                        crate::task::registry::TaskStatus::Cancelled,
                        Some("Cancelled by request.".into()),
                    )
                    .await;
                CallToolResult::success(vec![ContentBlock::text(format!(
                    "Task '{task_id}' cancelled."
                ))])
            }
            "extend_lease" => {
                let task_id = match params.task_id {
                    Some(ref id) => id,
                    None => {
                        return CallToolResult::error(vec![ContentBlock::text(
                            "Error: 'task_id' parameter is required for extend_lease.",
                        )]);
                    }
                };
                match registry.extend_budget(task_id, 25).await {
                    Ok(new_budget) => CallToolResult::success(vec![ContentBlock::text(format!(
                        "Budget for '{task_id}' extended to {new_budget} turns."
                    ))]),
                    Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
                        "Failed to extend budget: {e}"
                    ))]),
                }
            }
            "stats" => {
                let stats = crate::telemetry::derive_stats(state.root(), &Default::default());
                let text = format!(
                    "# Castor Telemetry Stats\n\
                     - total_prompt_tokens: {}\n\
                     - total_completion_tokens: {}\n\
                     - total_reasoning_tokens: {}\n\
                     - total_cached_tokens: {}\n\
                     - total_sessions: {}\n\
                     - total_turns: {}\n\
                     - total_tasks_completed: {}\n\
                     - total_tasks_failed: {}\n\
                     - total_tasks_cancelled: {}\n\
                     - total_tool_calls: {}\n\
                     - total_tool_errors: {}\n\
                     - estimated_cost_saved_usd: {:.2}\n\
                     - net_savings_usd: {:.2}\n\
                     - benchmark_model: {}\n",
                    stats.total_prompt_tokens,
                    stats.total_completion_tokens,
                    stats.total_reasoning_tokens,
                    stats.total_cached_tokens,
                    stats.total_sessions,
                    stats.total_turns,
                    stats.total_tasks_completed,
                    stats.total_tasks_failed,
                    stats.total_tasks_cancelled,
                    stats.total_tool_calls,
                    stats.total_tool_errors,
                    stats.estimated_cost_saved_usd,
                    stats.net_savings_usd,
                    stats.benchmark_model
                );
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            other => CallToolResult::error(vec![ContentBlock::text(format!(
                "Unknown action '{other}' for {}_task.",
                self.prefix
            ))]),
        }
    }

    async fn handle_server(&self, args: Option<Value>) -> CallToolResult {
        let params: ServerParams = match args {
            Some(v) => match serde_json::from_value(v) {
                Ok(p) => p,
                Err(e) => {
                    return CallToolResult::error(vec![ContentBlock::text(format!(
                        "Invalid server arguments: {e}"
                    ))]);
                }
            },
            None => {
                return CallToolResult::error(vec![ContentBlock::text(
                    "Missing arguments for server tool",
                )]);
            }
        };

        let loaded = match crate::config::load() {
            Ok(c) => c,
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Config error: {e}"
                ))]);
            }
        };
        let state = crate::state::StateDir::from_config(&loaded.config);
        let _ = state.ensure();

        match crate::run_server(&params.action, &loaded.config, &state).await {
            Ok((text, ok)) => {
                if ok {
                    CallToolResult::success(vec![ContentBlock::text(text)])
                } else {
                    CallToolResult::error(vec![ContentBlock::text(text)])
                }
            }
            Err(msg) => CallToolResult::error(vec![ContentBlock::text(msg)]),
        }
    }
}

impl ServerHandler for CastorMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            rmcp::model::Implementation::new("castor", env!("CARGO_PKG_VERSION"))
                .with_description("Castor: Rust MCP toolchain, proxy, and evo engine"),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let mut result = ListToolsResult::with_all_items(self.tools());
        result.ttl_ms = Some(300_000);
        result.cache_scope = Some(CacheScope::Public);
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let name = request.name.as_ref();
        let args_val = request.arguments.map(Value::Object);
        eprintln!("[castor-mcp] call_tool({name})");

        let result = match name {
            n if n == self.tool_name(TOOL_COWORKER) => self.handle_coworker(args_val).await,
            n if n == self.tool_name(TOOL_TASK) => self.handle_task(args_val).await,
            n if n == self.tool_name(TOOL_SERVER) => self.handle_server(args_val).await,
            _ => {
                return Err(rmcp::ErrorData::method_not_found::<
                    rmcp::model::CallToolRequestMethod,
                >());
            }
        };

        Ok(CallToolResponse::from(result))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools().into_iter().find(|t| t.name.as_ref() == name)
    }
}

/// Serve the MCP server over stdio.
///
/// stdout carries JSON-RPC frames; all logging goes to stderr.
pub async fn serve(prefix: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[castor-mcp] serving over stdio (prefix={prefix})");
    let (stdin, stdout) = stdio();
    let server = CastorMcpServer::new(prefix);
    let running = serve_server(server, (stdin, stdout)).await?;
    running.waiting().await?;
    eprintln!("[castor-mcp] stdio closed; shutting down");
    Ok(())
}

/// Build the user-facing long-poll wait commands (Windows `curl.exe` and
/// WSL `curl`) for a dispatched task.
///
/// Both embed an explicit `?timeout_s=3600` (1 hour). Without it the status
/// server falls back to its 30 s default and returns
/// `{status:"executing", timed_out:true}` with HTTP 200 (curl exits 0) while
/// the task is still running — a false "done" signal that makes callers
/// believe the task finished early.
pub fn wait_commands(status_port: u16, task_id: &str) -> (String, String) {
    let win = format!(
        "curl.exe -fsS --max-time 3600 --retry 5 --retry-delay 2 --retry-connrefused http://127.0.0.1:{status_port}/task/{task_id}/wait?timeout_s=3600"
    );
    let wsl = format!(
        "curl -fsS --max-time 3600 --retry 5 --retry-delay 2 --retry-connrefused http://127.0.0.1:{status_port}/task/{task_id}/wait?timeout_s=3600"
    );
    (win, wsl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------------
    // Long-poll wait command: must embed an explicit `?timeout_s=3600`
    // (otherwise the status server's 30 s default yields a premature
    // `{status:"executing", timed_out:true}` 200 response).
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Per-dispatch reasoning_effort tier validation — fail fast on values
    // the served template would 400 on, accept the known set (and absence).
    // ------------------------------------------------------------------

    #[test]
    fn validate_reasoning_effort_accepts_known_tiers_and_absence() {
        for tier in ["xhigh", "medium", "low", "  xhigh  "] {
            assert!(
                validate_reasoning_effort(Some(tier)).is_ok(),
                "tier {tier:?} must be accepted (whitespace trimmed)"
            );
        }
        assert!(validate_reasoning_effort(None).is_ok());
        assert!(validate_reasoning_effort(Some("")).is_ok());
        assert!(validate_reasoning_effort(Some("   ")).is_ok());
    }

    #[test]
    fn validate_reasoning_effort_rejects_wrong_case() {
        // The served template's valid set is lowercase; an uppercase tier
        // would 400 mid-run, so fail fast at dispatch.
        for tier in ["XHIGH", "Medium", "LOW"] {
            let msg = validate_reasoning_effort(Some(tier)).unwrap_err();
            assert!(msg.contains(tier), "error must name the bad value: {msg}");
        }
    }

    #[test]
    fn validate_reasoning_effort_rejects_invalid_tiers_with_clear_error() {
        for tier in ["bogus", "high", "off", "none", "minimal", "max", "xhigh2"] {
            let msg = validate_reasoning_effort(Some(tier)).unwrap_err(); // must reject
            assert!(msg.contains(tier), "error must name the bad value: {msg}");
            assert!(
                msg.contains("xhigh") && msg.contains("medium") && msg.contains("low"),
                "error must name the valid set: {msg}"
            );
        }
    }

    #[test]
    fn coworker_schema_lists_effort_tier_in_description() {
        // The JSON schema is generated from the field's doc comment; the
        // doc comment must keep advertising the valid tier set so MCP
        // clients can self-validate before dispatch.
        let server = CastorMcpServer::new(DEFAULT_TOOL_PREFIX);
        let coworker = server
            .tools()
            .into_iter()
            .find(|t| t.name.as_ref() == "castor_coworker")
            .expect("coworker tool present");
        let schema = coworker.input_schema.as_ref();
        let props = schema
            .get("properties")
            .and_then(|v| v.get("reasoning_effort"))
            .expect("reasoning_effort must be in the input schema properties");
        let field_desc = props
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            field_desc.contains("xhigh")
                && field_desc.contains("medium")
                && field_desc.contains("low"),
            "the field description must advertise the valid tier set: {field_desc}"
        );
    }

    #[test]
    fn wait_command_embeds_explicit_long_poll_timeout() {
        let (win, wsl) = wait_commands(8788, "task_abc123");

        for (name, cmd) in [("win", &win), ("wsl", &wsl)] {
            assert!(
                cmd.contains("http://127.0.0.1:8788/task/task_abc123/wait"),
                "{name} wait command must hit the status wait endpoint: {cmd}"
            );
            assert!(
                cmd.contains("?timeout_s=3600"),
                "{name} wait command must pass an explicit ?timeout_s=3600 (1 h) so a \
                 still-running task is not misreported as done via the 30 s default: {cmd}"
            );
            assert!(
                cmd.contains("--retry 5") && cmd.contains("--retry-connrefused"),
                "{name} wait command must retry connection refusals: {cmd}"
            );
        }
        // Windows flavor uses the .exe binary; WSL uses the bare curl.
        assert!(win.starts_with("curl.exe"));
        assert!(wsl.starts_with("curl "));
    }

    // ------------------------------------------------------------------
    // In-process: tools/list parity vs manifest constants
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn list_tools_matches_manifest_constants() {
        let server = CastorMcpServer::new(DEFAULT_TOOL_PREFIX);
        let tools = server.tools();

        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        let expected: Vec<String> = TOOL_BASE_NAMES
            .iter()
            .map(|b| format!("{DEFAULT_TOOL_PREFIX}_{b}"))
            .collect();
        assert_eq!(names, expected, "tool names must match manifest constants");

        for t in &tools {
            assert!(
                t.description.as_ref().is_some_and(|d| !d.is_empty()),
                "tool {} must have a description",
                t.name
            );
            let schema = t.input_schema.as_ref();
            assert!(
                schema.get("type").is_some(),
                "tool {} must have an input schema with a type",
                t.name
            );
        }

        let coworker = &tools[0];
        let schema = coworker.input_schema.as_ref();
        let required = schema.get("required").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or(""))
                .collect::<Vec<_>>()
        });
        assert!(
            required.as_deref() == Some(&["prompt"]),
            "coworker schema must require `prompt`, got {required:?}"
        );

        let task = &tools[1];
        let schema = task.input_schema.as_ref();
        let required = schema.get("required").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or(""))
                .collect::<Vec<_>>()
        });
        assert!(
            required.as_deref() == Some(&["action"]),
            "task schema must require `action`, got {required:?}"
        );

        let server_tool = &tools[2];
        let schema = server_tool.input_schema.as_ref();
        let required = schema.get("required").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or(""))
                .collect::<Vec<_>>()
        });
        assert!(
            required.as_deref() == Some(&["action"]),
            "server schema must require `action`, got {required:?}"
        );
    }

    // ------------------------------------------------------------------
    // stdio purity: spawn the binary, every stdout line is JSON-RPC
    // ------------------------------------------------------------------

    async fn spawn_and_handshake(
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> (
        tokio::process::Child,
        tokio::io::BufReader<tokio::process::ChildStdout>,
        tokio::process::ChildStdin,
    ) {
        let ext = std::env::consts::EXE_SUFFIX;
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/debug/castor{ext}"));
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(args);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        let state_dir = std::env::temp_dir().join(format!(
            "castor-mcp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&state_dir);
        cmd.env("CASTOR_STATE_DIR", state_dir.to_str().unwrap());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("failed to spawn castor binary");
        let stdout = child.stdout.take().expect("no stdout");
        let stdin = child.stdin.take().expect("no stdin");
        (child, tokio::io::BufReader::new(stdout), stdin)
    }

    async fn send_line(stdin: &mut tokio::process::ChildStdin, line: &str) {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("failed to write to stdin");
        stdin.flush().await.expect("failed to flush stdin");
    }

    async fn read_jsonrpc_line(
        reader: &mut tokio::io::BufReader<tokio::process::ChildStdout>,
    ) -> serde_json::Value {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("failed to read stdout line");
        let trimmed = line.trim();
        assert!(
            !trimmed.is_empty(),
            "stdout must not contain blank lines (JSON-RPC purity)"
        );
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .unwrap_or_else(|e| panic!("stdout line is not valid JSON: {trimmed:?} ({e})"));
        assert!(
            v.get("jsonrpc").is_some(),
            "every stdout line must be a JSON-RPC object, got: {v}"
        );
        v
    }

    #[tokio::test]
    async fn stdio_purity_initialize_and_tools_list() {
        let (mut child, mut reader, mut stdin) =
            spawn_and_handshake(&[], &[("CASTOR_SYNC_TIMEOUT_SECS", "0")]).await;

        // 1. initialize
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0.0"}}}"#,
        )
        .await;
        let init_resp = read_jsonrpc_line(&mut reader).await;
        assert_eq!(init_resp["id"], json!(1));
        assert!(
            init_resp.get("result").is_some(),
            "initialize must return a result, got: {init_resp}"
        );

        // 2. notifications/initialized
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;

        // 3. tools/list
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        )
        .await;
        let list_resp = read_jsonrpc_line(&mut reader).await;
        assert_eq!(list_resp["id"], json!(2));
        let tools = list_resp["result"]["tools"]
            .as_array()
            .expect("tools/list result must contain a tools array");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        let expected: Vec<String> = TOOL_BASE_NAMES
            .iter()
            .map(|b| format!("{DEFAULT_TOOL_PREFIX}_{b}"))
            .collect();
        assert_eq!(
            names, expected,
            "tools/list over stdio must match manifest constants"
        );

        // 4. tools/call coworker → real dispatch response
        send_line(
            &mut stdin,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"{}","arguments":{{"prompt":"hello test"}}}}}}"#,
                expected[0]
            ),
        )
        .await;
        let call_resp = read_jsonrpc_line(&mut reader).await;
        assert_eq!(call_resp["id"], json!(3));
        let text = call_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert!(
            text.contains("Castor Task Dispatched"),
            "coworker dispatch must return dispatch notification, got: {text}"
        );

        // Close stdin → server should exit cleanly.
        drop(stdin);
        let status = child.wait().await.expect("failed to wait for child");
        assert!(
            status.success(),
            "castor must exit cleanly after stdin closes, got: {status}"
        );
    }

    #[tokio::test]
    async fn stdio_purity_tool_prefix_env_override() {
        let (mut child, mut reader, mut stdin) =
            spawn_and_handshake(&[], &[("CASTOR_TOOL_PREFIX", "acme")]).await;

        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0.0"}}}"#,
        )
        .await;
        let _ = read_jsonrpc_line(&mut reader).await;
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        )
        .await;
        let list_resp = read_jsonrpc_line(&mut reader).await;
        let names: Vec<&str> = list_resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            vec!["acme_coworker", "acme_task", "acme_server"],
            "CASTOR_TOOL_PREFIX must override the tool-name prefix"
        );

        drop(stdin);
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn default_subcommand_is_mcp_server() {
        let (mut child, mut reader, mut stdin) = spawn_and_handshake(&[], &[]).await;

        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0.0"}}}"#,
        )
        .await;
        let init_resp = read_jsonrpc_line(&mut reader).await;
        assert_eq!(init_resp["id"], json!(1));
        assert!(
            init_resp.get("result").is_some(),
            "default subcommand must serve the MCP server, got: {init_resp}"
        );

        drop(stdin);
        let status = child.wait().await.expect("failed to wait for child");
        assert!(status.success());
    }

    #[tokio::test]
    async fn stdio_task_status_and_cancel() {
        let (mut child, mut reader, mut stdin) = spawn_and_handshake(&[], &[]).await;

        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.0.0"}}}"#,
        )
        .await;
        let _ = read_jsonrpc_line(&mut reader).await;
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await;

        // 1. Dispatch a task
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"castor_coworker","arguments":{"prompt":"check task test"}}}"#,
        )
        .await;
        let call_resp = read_jsonrpc_line(&mut reader).await;
        let text = call_resp["result"]["content"][0]["text"].as_str().unwrap();
        let task_id = text
            .split("/task/")
            .nth(1)
            .and_then(|s| s.split('/').next())
            .expect("must have task id in wait command");

        // 2. Query task status
        send_line(
            &mut stdin,
            &format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"castor_task","arguments":{{"action":"status","task_id":"{task_id}"}}}}}}"#
            ),
        )
        .await;
        let status_resp = read_jsonrpc_line(&mut reader).await;
        let status_text = status_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            status_text.contains(task_id),
            "status response must mention task_id"
        );

        // 3. Cancel task
        send_line(
            &mut stdin,
            &format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"castor_task","arguments":{{"action":"cancel","task_id":"{task_id}"}}}}}}"#
            ),
        )
        .await;
        let cancel_resp = read_jsonrpc_line(&mut reader).await;
        let cancel_text = cancel_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            cancel_text.contains("cancelled"),
            "cancel response must confirm cancellation, got: {cancel_text}"
        );

        // 4. Query stats
        send_line(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"castor_task","arguments":{"action":"stats"}}}"#,
        )
        .await;
        let stats_resp = read_jsonrpc_line(&mut reader).await;
        let stats_text = stats_resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            stats_text.contains("total_tasks"),
            "stats response must serialize Stats struct, got: {stats_text}"
        );

        drop(stdin);
        let status = child.wait().await.expect("child wait");
        assert!(status.success());
    }
}
