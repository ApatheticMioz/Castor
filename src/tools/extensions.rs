//! Per-dispatch MCP extension bridges.
//!
//! Spawns external MCP servers over stdio (via `rmcp::transport::TokioChildProcess`),
//! performs the MCP `initialize` handshake, lists their tools, and freezes the tool
//! set for the lifetime of the dispatch (vLLM APC: the tool set is frozen at
//! dispatch start; schemas are sent last).
//!
//! Each remote tool is exposed under a name-spaced name `<ext>__<tool>` so it can
//! be registered alongside the built-in tools without collisions.
//!
//! The tool set is frozen at `spawn` time: `tool_schemas()` and `call()` only ever
//! see the tools that were listed during the handshake. No re-listing happens.

use std::collections::HashMap;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::ServiceExt;
use rmcp::transport::TokioChildProcess;
use serde_json::Value;

use crate::engine::ToolSchema;

/// Maximum size (bytes) of a single tool-call result returned to the model.
/// Results beyond this are truncated and a marker is appended.
const RESULT_CAP_BYTES: usize = 20 * 1024;

/// Marker appended when a tool result is truncated to [`RESULT_CAP_BYTES`].
const TRUNCATION_MARKER: &str = "\n\n[... result truncated: exceeded 20KB cap ...]";

/// Handshake / per-call deadlines.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Typed, verbatim extension errors. These are never coerced into one another:
/// each failure mode maps to exactly one variant.
#[derive(Debug, thiserror::Error)]
pub enum ExtError {
    /// The child process could not be spawned (bad command, OS error).
    #[error("spawn failed: {0}")]
    SpawnFailed(String),
    /// The MCP `initialize` handshake or `tools/list` failed.
    #[error("handshake failed: {0}")]
    HandshakeFailed(String),
    /// A `call` referenced a tool that was not in the frozen set.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// A `tools/call` request failed (transport, protocol, or server error).
    #[error("call failed: {0}")]
    CallFailed(String),
    /// A tool result exceeded the size cap and could not be represented.
    #[error("result too large: {0} bytes (cap {1})")]
    ResultTooLarge(usize, usize),
}

/// A frozen tool: the raw server-side name plus its schema, captured once at
/// handshake time.
struct FrozenTool {
    raw_name: String,
    description: String,
    parameters: Value,
}

/// A single spawned MCP extension server and its frozen tool set.
struct ExtServer {
    /// Sanitized server name (used to namespace tool names).
    #[allow(dead_code)]
    name: String,
    /// The child's PID (captured before the transport is consumed by `serve`).
    pid: Option<u32>,
    /// The live rmcp client service (owns the child process + transport).
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    /// Frozen map: namespaced `<ext>__<tool>` -> frozen tool record.
    tools: HashMap<String, FrozenTool>,
}

/// A per-dispatch collection of MCP extension bridges.
///
/// The tool set is frozen at construction: after `spawn` returns, the set of
/// tools (and their schemas) is immutable for the life of the bridge.
pub struct ExtensionBridge {
    servers: Vec<ExtServer>,
}

impl ExtensionBridge {
    /// Spawn every stdio command, handshake each, list its tools, and freeze
    /// the tool set.
    ///
    /// A command that fails to spawn or handshake is skipped (logged to
    /// stderr) so a single bad extension can never take down the dispatch.
    /// Returns an error only if at least one command was requested and *none*
    /// could be brought up.
    pub async fn spawn(commands: &[String]) -> Result<Self, ExtError> {
        let mut servers = Vec::new();
        let mut any_failed = false;

        for cmd in commands {
            let tokens = tokenize(cmd);
            if tokens.is_empty() {
                eprintln!("[extensions] skipping empty command: {cmd:?}");
                any_failed = true;
                continue;
            }
            match Self::spawn_one(&tokens).await {
                Ok(server) => servers.push(server),
                Err(e) => {
                    eprintln!("[extensions] failed to start {cmd:?}: {e}");
                    any_failed = true;
                }
            }
        }

        if !commands.is_empty() && servers.is_empty() && any_failed {
            return Err(ExtError::HandshakeFailed(
                "no extensions could be started".to_string(),
            ));
        }
        Ok(Self { servers })
    }

    /// Spawn one extension, run the handshake, list its tools, and freeze them.
    async fn spawn_one(tokens: &[String]) -> Result<ExtServer, ExtError> {
        let command = &tokens[0];
        let args = &tokens[1..];

        // Build the child process. On Unix, make it the leader of its own
        // process group so a hung child can be killed as a group (no orphans).
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);

        let (transport, _stderr) = TokioChildProcess::builder(cmd)
            .spawn()
            .map_err(|e| ExtError::SpawnFailed(e.to_string()))?;
        // Capture the PID before `serve` consumes the transport.
        let pid = transport.id();

        // Handshake: `initialize` + `notifications/initialized`.
        let service = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| ExtError::HandshakeFailed("initialize timed out".to_string()))?
            .map_err(|e| ExtError::HandshakeFailed(e.to_string()))?;

        // List tools once; the set is frozen for the dispatch.
        let list = tokio::time::timeout(CALL_TIMEOUT, service.peer().list_tools(None))
            .await
            .map_err(|_| ExtError::HandshakeFailed("tools/list timed out".to_string()))?
            .map_err(|e| ExtError::HandshakeFailed(e.to_string()))?;

        let name = derive_server_name(command, args);
        let mut tools = HashMap::new();
        for t in list.tools {
            let raw = t.name.to_string();
            let namespaced = format!("{name}__{}", sanitize_name(&raw));
            let description = t
                .description
                .map(|d| d.to_string())
                .unwrap_or_else(|| format!("[ext:{name}] {raw}"));
            let parameters = Value::Object((*t.input_schema).clone());
            tools.insert(
                namespaced,
                FrozenTool {
                    raw_name: raw,
                    description,
                    parameters,
                },
            );
        }

        Ok(ExtServer {
            name,
            pid,
            service,
            tools,
        })
    }

    /// The frozen tool schemas across all servers, name-spaced `<ext>__<tool>`.
    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        let mut out = Vec::new();
        for s in &self.servers {
            for (namespaced, t) in &s.tools {
                out.push(ToolSchema {
                    name: namespaced.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                });
            }
        }
        out
    }

    /// Call a tool by its namespaced `<ext>__<tool>` name.
    ///
    /// `args_json` is a JSON object string of tool arguments. Returns the
    /// concatenated text content of the result, truncated to [`RESULT_CAP_BYTES`]
    /// with [`TRUNCATION_MARKER`] appended if it exceeds the cap.
    pub async fn call(&self, full_name: &str, args_json: &str) -> Result<String, ExtError> {
        let (server, frozen) = self
            .servers
            .iter()
            .find(|s| s.tools.contains_key(full_name))
            .and_then(|s| s.tools.get(full_name).map(|t| (s, t)))
            .ok_or_else(|| ExtError::UnknownTool(full_name.to_string()))?;

        let args: Value = if args_json.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(args_json).map_err(|e| ExtError::CallFailed(e.to_string()))?
        };
        let args_map = match args {
            Value::Object(m) => m,
            _ => {
                return Err(ExtError::CallFailed(
                    "arguments must be a JSON object".into(),
                ));
            }
        };

        let params = CallToolRequestParams::new(frozen.raw_name.clone()).with_arguments(args_map);
        let result = tokio::time::timeout(CALL_TIMEOUT, server.service.peer().call_tool(params))
            .await
            .map_err(|_| ExtError::CallFailed("call timed out".to_string()))?
            .map_err(|e| ExtError::CallFailed(e.to_string()))?;

        let text = result
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(cap_result(&text))
    }

    /// Gracefully shut down every child: cancel the service (closes the
    /// transport), wait for exit, and kill the process group on hang.
    pub async fn shutdown_all(self) {
        for mut s in self.servers {
            let pid = s.pid;
            let closed = matches!(
                tokio::time::timeout(SHUTDOWN_TIMEOUT, s.service.close()).await,
                Ok(Ok(_))
            );
            if !closed {
                kill_group(pid);
            }
        }
    }

    /// PIDs of all live children (for tests / liveness assertions).
    pub fn pids(&self) -> Vec<u32> {
        self.servers.iter().filter_map(|s| s.pid).collect()
    }
}

/// Truncate a result to [`RESULT_CAP_BYTES`], appending [`TRUNCATION_MARKER`].
fn cap_result(text: &str) -> String {
    if text.len() <= RESULT_CAP_BYTES {
        return text.to_string();
    }
    // Cut on a byte boundary that does not split a UTF-8 char.
    let mut end = RESULT_CAP_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Kill a process group (Unix) or the single process (Windows) by pid.
fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    unsafe {
        // The child was spawned with process_group(0), so its pgid == its pid.
        libc::killpg(pid as i32, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .spawn();
    }
}

/// Tokenize a command string on whitespace. No shell is ever invoked — this is
/// a pure split, so the command is never shell-interpolated.
fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace().map(|t| t.to_string()).collect()
}

/// Derive a server name from a command + args: the last non-flag token (the
/// package / entrypoint), stripped of scope + path segments.
fn derive_server_name(command: &str, args: &[String]) -> String {
    let candidates: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let pick = candidates.last().map(|s| s.as_str()).unwrap_or(command);
    let cleaned = pick.replace('@', "");
    let name = cleaned.rsplit('/').next().unwrap_or("ext");
    sanitize_name(name)
}

/// Sanitize an identifier to the safe charset `[a-z0-9_]`.
fn sanitize_name(s: &str) -> String {
    let mut out = String::new();
    let mut prev_underscore = false;
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "ext".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal stdio JSON-RPC MCP server (newline-delimited) implementing
    /// just enough of the protocol for the bridge to handshake and exercise
    /// tools: `initialize`, `notifications/initialized`, `tools/list`,
    /// `tools/call`. No network, no external deps.
    const MOCK_MCP_PY: &str = r#"
import sys, json

def write(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def handle(msg):
    id = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {}

    if method == "initialize":
        write({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-mcp", "version": "1.0.0"},
            },
        })
        return

    if method == "notifications/initialized":
        return

    if method == "tools/list":
        write({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echoes the provided text back.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    },
                    {
                        "name": "big",
                        "description": "Returns a large payload.",
                        "inputSchema": {"type": "object", "properties": {}},
                    },
                ]
            },
        })
        return

    if method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            text = "echo:" + str(args.get("text", ""))
        elif name == "big":
            text = "x" * 50000
        else:
            write({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "unknown tool: " + str(name)},
            })
            return
        write({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": text}], "isError": False},
        })
        return

    if id is not None:
        write({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not found: " + str(method)},
        })

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    handle(msg)
"#;

    /// Write the mock server to a temp file and return its path.
    fn write_mock() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "castor_ext_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mock_mcp.py");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(MOCK_MCP_PY.as_bytes()).unwrap();
        path
    }

    fn mock_command() -> String {
        let path = write_mock();
        let py = if cfg!(windows) { "python" } else { "python3" };
        format!("{py} {}", path.display())
    }

    /// Is a pid alive? (Unix: signal 0; Windows: tasklist probe.)
    fn is_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            unsafe { libc::kill(pid as i32, 0) == 0 }
        }
        #[cfg(not(unix))]
        {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
                .ok();
            out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
                .unwrap_or(false)
        }
    }

    #[tokio::test]
    async fn spawn_and_list_freezes_schemas() {
        let bridge = ExtensionBridge::spawn(&[mock_command()]).await.unwrap();
        let schemas = bridge.tool_schemas();
        // The mock exposes `echo` and `big`, name-spaced under the derived
        // server name (the `.py` basename, sanitized).
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.ends_with("__echo")),
            "expected a namespaced echo tool, got {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("__big")),
            "expected a namespaced big tool, got {names:?}"
        );
        // Schemas carry the input schema from the server.
        let echo = schemas.iter().find(|s| s.name.ends_with("__echo")).unwrap();
        assert_eq!(echo.parameters["type"], "object");
        assert!(echo.parameters["properties"]["text"].is_object());
        bridge.shutdown_all().await;
    }

    #[tokio::test]
    async fn call_returns_result() {
        let bridge = ExtensionBridge::spawn(&[mock_command()]).await.unwrap();
        let full = bridge
            .tool_schemas()
            .iter()
            .find(|s| s.name.ends_with("__echo"))
            .unwrap()
            .name
            .clone();
        let out = bridge.call(&full, r#"{"text":"hi"}"#).await.unwrap();
        assert_eq!(out, "echo:hi");
        bridge.shutdown_all().await;
    }

    #[tokio::test]
    async fn unknown_tool_is_typed_error() {
        let bridge = ExtensionBridge::spawn(&[mock_command()]).await.unwrap();
        let err = bridge.call("nope__doesnotexist", "{}").await.unwrap_err();
        assert!(
            matches!(err, ExtError::UnknownTool(ref n) if n == "nope__doesnotexist"),
            "expected UnknownTool, got {err:?}"
        );
        bridge.shutdown_all().await;
    }

    #[tokio::test]
    async fn oversized_result_is_truncated_with_marker() {
        let bridge = ExtensionBridge::spawn(&[mock_command()]).await.unwrap();
        let full = bridge
            .tool_schemas()
            .iter()
            .find(|s| s.name.ends_with("__big"))
            .unwrap()
            .name
            .clone();
        let out = bridge.call(&full, "{}").await.unwrap();
        assert!(
            out.len() > RESULT_CAP_BYTES,
            "expected a result at/over the cap, got {} bytes",
            out.len()
        );
        assert!(
            out.ends_with(TRUNCATION_MARKER),
            "expected the truncation marker, got tail: {}",
            &out[out.len().saturating_sub(60)..]
        );
        // The payload is capped: total length is cap + marker (minus any
        // char-boundary backoff, which is at most a few bytes).
        assert!(
            out.len() <= RESULT_CAP_BYTES + TRUNCATION_MARKER.len() + 4,
            "result not capped: {} bytes",
            out.len()
        );
        bridge.shutdown_all().await;
    }

    #[tokio::test]
    async fn shutdown_all_exits_children_no_orphans() {
        let bridge = ExtensionBridge::spawn(&[mock_command()]).await.unwrap();
        let pids = bridge.pids();
        assert!(!pids.is_empty(), "expected at least one child pid");
        for p in &pids {
            assert!(is_alive(*p), "child {p} should be alive before shutdown");
        }
        bridge.shutdown_all().await;
        // Give the OS a moment to reap, then confirm no orphans remain.
        tokio::time::sleep(Duration::from_millis(200)).await;
        for p in &pids {
            assert!(!is_alive(*p), "child {p} should be dead after shutdown");
        }
    }

    #[tokio::test]
    async fn spawn_failure_is_typed() {
        // A command that does not exist -> SpawnFailed (skipped), and since it
        // is the only command, spawn reports HandshakeFailed("no extensions...").
        let res = ExtensionBridge::spawn(&["definitely_not_a_real_cmd_xyz".to_string()]).await;
        assert!(res.is_err(), "expected an error for a bad command");
    }
}
