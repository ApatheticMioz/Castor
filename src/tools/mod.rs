//! Tool registry and dispatch.
//!
//! Provides [`CompositeExecutor`], the aggregate implementation of
//! [`crate::runner::ToolExecutor`] that routes tool calls to filesystem,
//! sandboxed shell, ast-grep, web search/fetch, or stdio extensions.

pub mod ast;
pub mod extensions;
pub mod fs;
pub mod sandbox;
pub mod shell;
pub mod web;

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use self::extensions::ExtensionBridge;
use self::fs::FsExecutor;
use self::web::WebClient;
use crate::engine::ToolSchema;
use crate::runner::{ToolError, ToolExecutor, ToolOutcome};

/// The aggregate tool executor providing all built-in capabilities and extensions.
pub struct CompositeExecutor {
    workspace_root: PathBuf,
    fs: FsExecutor,
    web: WebClient,
    brave_api_key: Option<String>,
    extensions: Option<ExtensionBridge>,
}

impl CompositeExecutor {
    /// Create a new executor for the given workspace root, with optional extension bridge.
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        extensions: Option<ExtensionBridge>,
    ) -> Result<Self, fs::FsError> {
        Self::with_config(workspace_root, None, None, extensions)
    }

    /// Create a new executor with explicit search configuration.
    pub fn with_config(
        workspace_root: impl Into<PathBuf>,
        searxng_url: Option<String>,
        brave_api_key: Option<String>,
        extensions: Option<ExtensionBridge>,
    ) -> Result<Self, fs::FsError> {
        let root = crate::platform::to_host_path(workspace_root.into());
        let state_dir = crate::config::load().ok().map(|l| l.config.state_dir);
        let policy = sandbox::SandboxPolicy::for_workspace(&root, state_dir.as_deref());
        let fs = FsExecutor::with_policy(policy);
        let searx_base =
            searxng_url.or_else(|| crate::tools::web::DEFAULT_SEARXNG.map(str::to_string));
        let web = WebClient::with_base_urls(searx_base, None, None);
        Ok(Self {
            fs,
            workspace_root: root,
            web,
            brave_api_key,
            extensions,
        })
    }

    /// Set an explicit WebClient (useful for testing with mock base URLs).
    pub fn with_web_client(mut self, web: WebClient) -> Self {
        self.web = web;
        self
    }

    /// Return the list of all available tool schemas (builtins + extensions).
    pub fn tool_schemas(&self) -> Vec<ToolSchema> {
        let mut schemas = builtin_tool_schemas();
        if let Some(ref ext) = self.extensions {
            schemas.extend(ext.tool_schemas());
        }
        schemas
    }
}

/// The standard set of built-in tool schemas exposed to the model.
pub fn builtin_tool_schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "read_file".to_string(),
            description: "Read a file from disk. Can read whole files or specific line ranges.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path" },
                    "start_line": { "type": "integer", "description": "1-based starting line number (optional)" },
                    "end_line": { "type": "integer", "description": "1-based ending line number (optional)" }
                },
                "required": ["path"]
            }),
        },
        ToolSchema {
            name: "write_file".to_string(),
            description: "Write content to a file. Overwrites existing files by default.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative or absolute file path" },
                    "content": { "type": "string", "description": "Full file content to write" },
                    "overwrite": { "type": "boolean", "description": "Whether to overwrite existing file (default: true)" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSchema {
            name: "edit_file".to_string(),
            description: "Make an exact substring replacement in a file. Validates syntax before saving.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Target file path" },
                    "target_content": { "type": "string", "description": "Exact text to find and replace" },
                    "replacement_content": { "type": "string", "description": "Exact replacement text" },
                    "replace_all": { "type": "boolean", "description": "Replace all occurrences if true (default: false)" }
                },
                "required": ["path", "target_content", "replacement_content"]
            }),
        },
        ToolSchema {
            name: "list_dir".to_string(),
            description: "List directory contents up to a maximum depth.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path (default: current workspace)" },
                    "max_depth": { "type": "integer", "description": "Maximum directory traversal depth (default: 2)" }
                }
            }),
        },
        ToolSchema {
            name: "search_code".to_string(),
            description: "Search for text patterns across files in the workspace.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Text pattern to search for" },
                    "path": { "type": "string", "description": "Subdirectory to limit search (default: current workspace)" },
                    "max_results": { "type": "integer", "description": "Maximum matches to return (default: 50)" }
                },
                "required": ["query"]
            }),
        },
        ToolSchema {
            name: "bash".to_string(),
            description: "Execute a command in bash shell with layered safety checks and process group isolation.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command line to execute" },
                    "timeout_ms": { "type": "integer", "description": "Execution timeout in milliseconds (default: 30000)" }
                },
                "required": ["command"]
            }),
        },
        ToolSchema {
            name: "ast_search".to_string(),
            description: "Search code using an ast-grep structural pattern.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "ast-grep pattern (e.g. 'function $NAME($$$ARGS) { $$$BODY }')" },
                    "lang": { "type": "string", "description": "Target language (js, ts, py, rs, etc.)" },
                    "path": { "type": "string", "description": "Optional file or directory path to search within (defaults to workspace root)" }
                },
                "required": ["pattern", "lang"]
            }),
        },
        ToolSchema {
            name: "ast_replace".to_string(),
            description: "Search and replace code using an ast-grep structural pattern.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "ast-grep pattern to match" },
                    "replacement": { "type": "string", "description": "Replacement pattern" },
                    "lang": { "type": "string", "description": "Target language" },
                    "path": { "type": "string", "description": "Optional file or directory path to replace within (defaults to workspace root)" }
                },
                "required": ["pattern", "replacement", "lang"]
            }),
        },
        ToolSchema {
            name: "web_search".to_string(),
            description: "Search the web using SearXNG with Brave and DuckDuckGo fallbacks.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "num_results": { "type": "integer", "description": "Number of results to return (default: 10)" }
                },
                "required": ["query"]
            }),
        },
        ToolSchema {
            name: "web_fetch".to_string(),
            description: "Fetch a web page or document and extract clean readable markdown.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" }
                },
                "required": ["url"]
            }),
        },
    ]
}

#[async_trait]
impl ToolExecutor for CompositeExecutor {
    async fn execute(&self, name: &str, args_json: &str) -> Result<ToolOutcome, ToolError> {
        let args: Value = serde_json::from_str(args_json).map_err(|e| ToolError::Execute {
            name: name.to_string(),
            message: format!("invalid JSON args: {e}"),
        })?;

        match name {
            // Filesystem tools
            "read_file" | "write_file" | "edit_file" | "list_dir" | "search_code" => {
                self.fs.execute(name, args_json).await
            }

            // Shell execution
            "bash" => {
                let cmd = args
                    .get("command")
                    .or(args.get("cmd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                let timeout = Duration::from_millis(timeout_ms);

                match shell::run_async(cmd, &self.workspace_root, timeout).await {
                    Ok(out) => {
                        let mut text = String::new();
                        if out.exit_code != 0 {
                            text.push_str(&format!("(exit code {})\n", out.exit_code));
                        }
                        if !out.stdout.is_empty() {
                            text.push_str(&out.stdout);
                        }
                        if !out.stderr.is_empty() {
                            if !text.is_empty() && !text.ends_with('\n') {
                                text.push('\n');
                            }
                            text.push_str("stderr:\n");
                            text.push_str(&out.stderr);
                        }
                        if text.is_empty() {
                            text = "(empty output)".to_string();
                        }
                        if out.truncated {
                            text.push_str("\n[output truncated]");
                        }
                        Ok(ToolOutcome { text })
                    }
                    Err(e) => Ok(ToolOutcome {
                        text: format!("Error: {e}"),
                    }),
                }
            }

            // AST tools
            "ast_search" => {
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let lang = args.get("lang").and_then(|v| v.as_str()).unwrap_or("");
                let path = args.get("path").and_then(|v| v.as_str());
                match ast::ast_search(&self.workspace_root, pattern, lang, path) {
                    Ok(matches) => {
                        if matches.is_empty() {
                            Ok(ToolOutcome {
                                text: "No matches found.".to_string(),
                            })
                        } else {
                            let lines: Vec<String> = matches
                                .into_iter()
                                .map(|m| format!("{}:{}: {}", m.file, m.line, m.text.trim()))
                                .collect();
                            Ok(ToolOutcome {
                                text: lines.join("\n"),
                            })
                        }
                    }
                    Err(e) => Ok(ToolOutcome {
                        text: format!("Error: {e}"),
                    }),
                }
            }

            "ast_replace" => {
                let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let replacement = args
                    .get("replacement")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let lang = args.get("lang").and_then(|v| v.as_str()).unwrap_or("");
                let path = args.get("path").and_then(|v| v.as_str());

                match ast::ast_replace(&self.workspace_root, pattern, replacement, lang, path) {
                    Ok(summary) => Ok(ToolOutcome {
                        text: format!(
                            "Replaced {} occurrence(s) across {} file(s) (applied: {}, rolled back: {}).",
                            summary.replacements,
                            summary.files_applied,
                            summary.applied.len(),
                            summary.rolled_back.len()
                        ),
                    }),
                    Err(e) => Ok(ToolOutcome {
                        text: format!("Error: {e}"),
                    }),
                }
            }

            // Web tools
            "web_search" => {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let limit = args
                    .get("num_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10) as usize;
                match self
                    .web
                    .web_search(query, self.brave_api_key.as_deref())
                    .await
                {
                    Ok(outcome) => {
                        if outcome.results.is_empty() {
                            Ok(ToolOutcome {
                                text: "No search results found.".to_string(),
                            })
                        } else {
                            let mut lines = Vec::new();
                            for (i, r) in outcome.results.iter().take(limit).enumerate() {
                                lines.push(format!(
                                    "{}. [{}]({})\n   {}",
                                    i + 1,
                                    r.title,
                                    r.url,
                                    r.snippet
                                ));
                            }
                            Ok(ToolOutcome {
                                text: lines.join("\n\n"),
                            })
                        }
                    }
                    Err(e) => Ok(ToolOutcome {
                        text: format!("Error: {e}"),
                    }),
                }
            }

            "web_fetch" => {
                let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
                match self.web.fetch_docs(url).await {
                    Ok(res) => Ok(ToolOutcome { text: res.markdown }),
                    Err(e) => Ok(ToolOutcome {
                        text: format!("Error: {e}"),
                    }),
                }
            }

            // Dynamic extensions
            other => {
                if let Some(ref ext) = self.extensions {
                    match ext.call(other, args_json).await {
                        Ok(text) => Ok(ToolOutcome { text }),
                        Err(e) => Ok(ToolOutcome {
                            text: format!("Error: {e}"),
                        }),
                    }
                } else {
                    Err(ToolError::Execute {
                        name: name.to_string(),
                        message: format!("unknown tool: {name}"),
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "castor_tools_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn composite_executor_executes_fs_and_bash() {
        let ws = temp_workspace();
        let executor = CompositeExecutor::new(&ws, None).expect("failed to create executor");

        // Check schemas list
        let schemas = executor.tool_schemas();
        assert_eq!(schemas.len(), 10);
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"ast_search"));
        assert!(names.contains(&"web_search"));

        // Write a file
        let write_res = executor
            .execute(
                "write_file",
                r#"{"path": "hello.txt", "content": "hello world"}"#,
            )
            .await
            .unwrap();
        assert!(write_res.text.contains("hello.txt"));

        // Read the file back
        let read_res = executor
            .execute("read_file", r#"{"path": "hello.txt"}"#)
            .await
            .unwrap();
        assert!(read_res.text.contains("hello world"));

        // Execute bash
        let bash_res = executor
            .execute("bash", r#"{"command": "echo 'from bash'"}"#)
            .await
            .unwrap();
        assert!(bash_res.text.contains("from bash"));

        // Unknown tool returns error
        let err = executor
            .execute("non_existent_tool", r#"{}"#)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execute { .. }));

        let _ = std::fs::remove_dir_all(&ws);
    }
}
