//! OpenAI-compatible chat client: streaming SSE and non-streaming.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Instant;

use futures_util::StreamExt;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::Config;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("missing {field} in config")]
    MissingConfig { field: &'static str },
    #[error("upstream HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("connection to {url} failed: {source}")]
    Connect {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("invalid SSE frame: {0}")]
    Sse(String),
    #[error("malformed upstream response: {0}")]
    Malformed(String),
    /// A replayed trace ran out of recorded responses before the runner
    /// finished. A clean, typed engine failure — never improvised content.
    #[error("trace exhausted: no more recorded responses")]
    TraceExhausted,
}

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Present on assistant messages that carry tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Present on tool-role messages; the id of the tool call being answered.
    pub tool_call_id: Option<String>,
}

impl Message {
    /// Serialize to the OpenAI chat-completion message shape.
    pub fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("role".into(), json!(self.role));
        if self.role == "assistant" && self.content.is_empty() {
            obj.insert("content".into(), Value::Null);
        } else if self.role == "user"
            && self
                .content
                .trim_start()
                .starts_with("[Visual Inspection Attachment: data:image/")
        {
            let trimmed = self.content.trim();
            if let Some(data_url) = trimmed
                .strip_prefix("[Visual Inspection Attachment: ")
                .and_then(|s| s.strip_suffix(']'))
            {
                obj.insert(
                    "content".into(),
                    json!([
                        {
                            "type": "text",
                            "text": "Inspect the attached image:"
                        },
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": data_url.trim()
                            }
                        }
                    ]),
                );
            } else {
                obj.insert("content".into(), json!(self.content));
            }
        } else {
            obj.insert("content".into(), json!(self.content));
        }
        if !self.tool_calls.is_empty() {
            obj.insert(
                "tool_calls".into(),
                json!(
                    self.tool_calls
                        .iter()
                        .map(|tc| tc.to_json())
                        .collect::<Vec<_>>()
                ),
            );
        }
        if let Some(id) = &self.tool_call_id {
            obj.insert("tool_call_id".into(), json!(id));
        }
        Value::Object(obj)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolCall {
    pub index: usize,
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// Serialize to the OpenAI tool-call shape (for assistant messages).
    pub fn to_json(&self) -> Value {
        let args = if self.arguments.trim().is_empty() {
            "{}".to_string()
        } else {
            self.arguments.clone()
        };
        json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": args,
            },
        })
    }
}

/// An OpenAI function tool schema, sent to the engine in the `tools` field.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSchema {
    pub fn to_json(&self) -> Value {
        let params = if self.parameters.is_null() || self.parameters == json!({}) {
            json!({ "type": "object", "properties": {} })
        } else {
            self.parameters.clone()
        };
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": params,
            },
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub ttft_ms: Option<f64>,
    pub total_ms: f64,
    pub tokens_per_sec: Option<f64>,
    /// Engine-reported prompt-token count for the turn (from the `usage`
    /// object). `None` when the engine did not report it.
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub metrics: Metrics,
}

pub struct EngineClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    pub backend: super::backend::Backend,
    pub capabilities: super::backend::EngineCapabilities,
}

impl EngineClient {
    pub fn from_config(cfg: &Config) -> Result<Self, EngineError> {
        let base_url = cfg
            .base_url
            .clone()
            .ok_or(EngineError::MissingConfig { field: "base_url" })?;
        let model = cfg
            .model
            .clone()
            .ok_or(EngineError::MissingConfig { field: "model" })?;
        let http = reqwest::Client::builder()
            .tcp_keepalive(Some(std::time::Duration::from_secs(15)))
            .build()
            .map_err(|e| EngineError::Connect {
                url: base_url.clone(),
                source: e,
            })?;
        let backend = super::backend::Backend::from_config(cfg);
        let capabilities = backend.capabilities();
        Ok(Self {
            http,
            base_url,
            model,
            api_key: cfg.api_key.clone(),
            backend,
            capabilities,
        })
    }

    /// Send one chat-completion request, consuming the response.
    ///
    /// Reasoning effort uses the selected engine's request format.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        stream: bool,
        reasoning_effort: Option<&str>,
    ) -> Result<Completion, EngineError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut body = json!({
            "model": self.model,
            "messages": messages.iter().map(|m| m.to_json()).collect::<Vec<_>>(),
            "stream": stream,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools.iter().map(|t| t.to_json()).collect::<Vec<_>>());
            body["tool_choice"] = json!("auto");
        }
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(effort) = reasoning_effort {
            match self.backend.effort_serializer() {
                super::backend::EffortSerializer::TopLevel => {
                    let normalized = match effort.trim().to_ascii_lowercase().as_str() {
                        "high" | "xhigh" => Some("xhigh"),
                        "medium" | "med" => Some("medium"),
                        "low" => Some("low"),
                        "none" | "off" | "false" | "0" | "" => None,
                        _ => Some("medium"),
                    };
                    if let Some(tier) = normalized {
                        body["reasoning_effort"] = json!(tier);
                    }
                }
                super::backend::EffortSerializer::ChatTemplateKwargs => {
                    body["chat_template_kwargs"] = json!({ "reasoning_effort": effort });
                }
            }
        }
        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let t0 = Instant::now();
        let resp = req.send().await.map_err(|e| EngineError::Connect {
            url: url.clone(),
            source: e,
        })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(EngineError::Http { status, body });
        }
        if stream {
            self.consume_stream(resp, t0).await
        } else {
            self.parse_non_stream(resp, t0).await
        }
    }

    async fn consume_stream(
        &self,
        resp: reqwest::Response,
        t0: Instant,
    ) -> Result<Completion, EngineError> {
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut reasoning_content = String::new();
        let mut finish_reason: Option<String> = None;
        let mut ttft: Option<std::time::Duration> = None;
        let mut prompt_tokens: Option<u64> = None;
        let mut cached_tokens: Option<u64> = None;
        let mut completion_tokens: Option<u64> = None;
        let mut reasoning_tokens: Option<u64> = None;
        let mut tool_calls: BTreeMap<usize, ToolCall> = BTreeMap::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| EngineError::Sse(e.to_string()))?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(payload) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" {
                    let rc = if reasoning_content.trim().is_empty() {
                        None
                    } else {
                        Some(reasoning_content)
                    };
                    return Ok(assemble(
                        &content,
                        rc,
                        tool_calls,
                        finish_reason,
                        ttft,
                        prompt_tokens,
                        cached_tokens,
                        completion_tokens,
                        reasoning_tokens,
                        t0,
                    ));
                }
                if payload.is_empty() {
                    continue;
                }
                let frame: Value =
                    serde_json::from_str(payload).map_err(|e| EngineError::Sse(e.to_string()))?;
                if let Some(error) = frame.get("error") {
                    return Err(EngineError::Sse(format!("upstream error: {error}")));
                }
                if let Some(usage) = frame.get("usage") {
                    // Engine-reported usage chunk: completion tokens take
                    // precedence over the local char estimate; prompt tokens
                    // are carried through for the session ledger.
                    if let Some(c) = usage.get("completion_tokens").and_then(Value::as_u64) {
                        completion_tokens = Some(c);
                    }
                    if let Some(p) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                        prompt_tokens = Some(p);
                    }
                    if let Some(r) = usage
                        .get("completion_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(Value::as_u64)
                    {
                        reasoning_tokens = Some(r);
                    }
                    if let Some(k) = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(Value::as_u64)
                        .or_else(|| usage.get("cache_read_input_tokens").and_then(Value::as_u64))
                        .or_else(|| usage.get("cached_prompt_tokens").and_then(Value::as_u64))
                        .or_else(|| usage.get("cache_read_tokens").and_then(Value::as_u64))
                    {
                        cached_tokens = Some(k);
                    }
                }
                for choice in frame
                    .get("choices")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                        finish_reason = Some(fr.to_string());
                    }
                    let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) else {
                        continue;
                    };
                    if let Some(r) = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .and_then(Value::as_str)
                    {
                        if !r.is_empty() && ttft.is_none() {
                            ttft = Some(t0.elapsed());
                        }
                        reasoning_content.push_str(r);
                    }
                    if let Some(c) = delta.get("content").and_then(Value::as_str) {
                        if !c.is_empty() && ttft.is_none() {
                            ttft = Some(t0.elapsed());
                        }
                        content.push_str(c);
                    }
                    if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                        for tc in tcs {
                            let Some(idx) = tc.get("index").and_then(Value::as_u64) else {
                                continue;
                            };
                            if ttft.is_none() {
                                ttft = Some(t0.elapsed());
                            }
                            let slot = tool_calls.entry(idx as usize).or_default();
                            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                                slot.id = id.to_string();
                            }
                            let fn_ = tc.get("function");
                            if let Some(name) =
                                fn_.and_then(|f| f.get("name")).and_then(Value::as_str)
                            {
                                slot.name = name.to_string();
                            }
                            if let Some(args) =
                                fn_.and_then(|f| f.get("arguments")).and_then(Value::as_str)
                            {
                                slot.arguments.push_str(args);
                            }
                        }
                    }
                }
            }
        }
        if self.capabilities.stream_sentinel == super::backend::StreamSentinel::RequiresDone {
            return Err(EngineError::Sse(
                "stream ended without [DONE] sentinel".into(),
            ));
        }
        let rc = if reasoning_content.trim().is_empty() {
            None
        } else {
            Some(reasoning_content)
        };
        Ok(assemble(
            &content,
            rc,
            tool_calls,
            finish_reason,
            ttft,
            prompt_tokens,
            cached_tokens,
            completion_tokens,
            reasoning_tokens,
            t0,
        ))
    }

    async fn parse_non_stream(
        &self,
        resp: reqwest::Response,
        t0: Instant,
    ) -> Result<Completion, EngineError> {
        let body: Value = resp
            .json()
            .await
            .map_err(|e| EngineError::Malformed(e.to_string()))?;
        let choice = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| EngineError::Malformed("no choices in response".into()))?;
        let msg = choice.get("message").unwrap_or(&Value::Null);
        let content = msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(String::from);
        let mut tool_calls = BTreeMap::new();
        if let Some(tcs) = msg.get("tool_calls").and_then(Value::as_array) {
            for (i, tc) in tcs.iter().enumerate() {
                let mut slot = ToolCall {
                    index: i,
                    ..Default::default()
                };
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    slot.id = id.to_string();
                }
                let fn_ = tc.get("function");
                if let Some(name) = fn_.and_then(|f| f.get("name")).and_then(Value::as_str) {
                    slot.name = name.to_string();
                }
                if let Some(args) = fn_.and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                    slot.arguments = args.to_string();
                }
                tool_calls.insert(i, slot);
            }
        }
        let usage = body.get("usage");
        let prompt_tokens = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64);
        let cached_tokens = usage.and_then(|u| {
            u.get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_u64)
                .or_else(|| u.get("cache_read_input_tokens").and_then(Value::as_u64))
                .or_else(|| u.get("cached_prompt_tokens").and_then(Value::as_u64))
                .or_else(|| u.get("cache_read_tokens").and_then(Value::as_u64))
        });
        let completion_tokens = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_u64);
        let reasoning_tokens = usage
            .and_then(|u| u.get("completion_tokens_details"))
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64);
        let ttft = if content.is_empty() && tool_calls.is_empty() {
            None
        } else {
            Some(t0.elapsed())
        };
        let reasoning_content = msg
            .get("reasoning_content")
            .or_else(|| msg.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(String::from);
        Ok(assemble(
            &content,
            reasoning_content,
            tool_calls,
            finish_reason,
            ttft,
            prompt_tokens,
            cached_tokens,
            completion_tokens,
            reasoning_tokens,
            t0,
        ))
    }
}

static RE_THINK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?s)<think>.*?</think>").expect("valid think regex"));
static RE_TOOL_CALL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?s)<tool_call>(.*?)</tool_call>").expect("valid tool_call regex")
});
static RE_FUNC: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?s)<function=([a-zA-Z0-9_\-]+)>(.*?)</function>")
        .expect("valid function regex")
});
static RE_PARAM: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?s)<parameter=([a-zA-Z0-9_\-]+)>(.*?)</parameter>")
        .expect("valid parameter regex")
});

fn extract_fallback_tool_calls(raw_content: &str) -> (String, Vec<ToolCall>) {
    let mut tool_calls = Vec::new();

    // Identify <think>...</think> spans so hypothetical or deliberative tool calls
    // mentioned within reasoning traces are never dispatched as real execution steps.
    let think_spans: Vec<(usize, usize)> = RE_THINK
        .find_iter(raw_content)
        .map(|m| (m.start(), m.end()))
        .collect();
    let is_inside_think = |start: usize| {
        think_spans
            .iter()
            .any(|(ts, te)| start >= *ts && start < *te)
    };

    for cap in RE_TOOL_CALL.captures_iter(raw_content) {
        let full_match = match cap.get(0) {
            Some(m) => m,
            None => continue,
        };
        if is_inside_think(full_match.start()) {
            continue;
        }

        let inner = cap.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
        if inner.is_empty() {
            continue;
        }

        // Branch 1: Qwen / Bonsai XML function format (<function=NAME>...</function>)
        if let Some(func_cap) = RE_FUNC.captures(inner) {
            let func_name = func_cap
                .get(1)
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            let body = func_cap.get(2).map(|m| m.as_str()).unwrap_or_default();

            let mut params_map = serde_json::Map::new();
            for p_cap in RE_PARAM.captures_iter(body) {
                let p_name = p_cap.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
                let p_val_raw = p_cap.get(2).map(|m| m.as_str()).unwrap_or_default();
                let mut p_val = p_val_raw;
                if let Some(stripped) = p_val.strip_prefix("\r\n") {
                    p_val = stripped;
                } else if let Some(stripped) = p_val.strip_prefix('\n') {
                    p_val = stripped;
                }
                if let Some(stripped) = p_val.strip_suffix("\r\n") {
                    p_val = stripped;
                } else if let Some(stripped) = p_val.strip_suffix('\n') {
                    p_val = stripped;
                }

                let trimmed = p_val.trim();
                let val_json: Value = if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    serde_json::from_str(trimmed)
                        .unwrap_or_else(|_| Value::String(p_val.to_string()))
                } else if matches!(
                    p_name,
                    "start_line"
                        | "end_line"
                        | "max_depth"
                        | "timeout_ms"
                        | "max_results"
                        | "limit"
                ) && let Ok(n) = trimmed.parse::<i64>()
                {
                    Value::Number(n.into())
                } else if matches!(p_name, "overwrite" | "replace_all" | "fuzzy")
                    && let Ok(b) = trimmed.parse::<bool>()
                {
                    Value::Bool(b)
                } else {
                    Value::String(p_val.to_string())
                };

                params_map.insert(p_name.to_string(), val_json);
            }

            let args_str = if params_map.is_empty() {
                if let Ok(Value::Object(map)) = serde_json::from_str(body.trim()) {
                    Value::Object(map).to_string()
                } else {
                    "{}".to_string()
                }
            } else {
                Value::Object(params_map).to_string()
            };

            let index = tool_calls.len();
            tool_calls.push(ToolCall {
                id: format!("call_fallback_{index}"),
                name: func_name.to_string(),
                arguments: args_str,
                index,
            });
        }
        // Branch 2: Hermes / DeepSeek / Mistral / Llama-3 JSON format
        else if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(inner) {
            let (name_opt, args_val) = if let Some(name) = obj.get("name").and_then(Value::as_str) {
                (
                    Some(name),
                    obj.get("arguments").or_else(|| obj.get("parameters")),
                )
            } else if let Some(func) = obj.get("function").and_then(Value::as_object) {
                (
                    func.get("name").and_then(Value::as_str),
                    func.get("arguments").or_else(|| func.get("parameters")),
                )
            } else {
                (None, None)
            };

            if let Some(name) = name_opt {
                let args_str = if let Some(args) = args_val {
                    if let Some(s) = args.as_str() {
                        s.to_string()
                    } else {
                        args.to_string()
                    }
                } else {
                    "{}".to_string()
                };

                let index = tool_calls.len();
                tool_calls.push(ToolCall {
                    id: format!("call_fallback_{index}"),
                    name: name.to_string(),
                    arguments: args_str,
                    index,
                });
            }
        }
    }

    if tool_calls.is_empty() {
        (raw_content.to_string(), tool_calls)
    } else {
        // Strip only the tool_call blocks that were outside <think>
        let mut stripped = String::new();
        let mut last_end = 0;
        for m in RE_TOOL_CALL.find_iter(raw_content) {
            if !is_inside_think(m.start()) {
                stripped.push_str(&raw_content[last_end..m.start()]);
                last_end = m.end();
            }
        }
        stripped.push_str(&raw_content[last_end..]);
        (stripped.trim().to_string(), tool_calls)
    }
}

#[allow(clippy::too_many_arguments)] // each argument is a distinct per-turn metric
fn assemble(
    content: &str,
    reasoning_content: Option<String>,
    mut tool_calls: BTreeMap<usize, ToolCall>,
    mut finish_reason: Option<String>,
    ttft: Option<std::time::Duration>,
    prompt_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    t0: Instant,
) -> Completion {
    let mut final_content = content.to_string();

    // Fallback tool call extraction: if upstream server emitted raw XML or JSON
    // tool calls inside content (common with llama.cpp, Qwen chat templates,
    // or when reasoning tokens precede function calls), extract them into structured ToolCalls.
    if tool_calls.is_empty() {
        let (stripped, fallback_tcs) = extract_fallback_tool_calls(content);
        if !fallback_tcs.is_empty() {
            final_content = stripped;
            for tc in fallback_tcs {
                tool_calls.insert(tc.index, tc);
            }
            if finish_reason.is_none() || finish_reason.as_deref() == Some("stop") {
                finish_reason = Some("tool_calls".to_string());
            }
        }
    }

    let total = t0.elapsed();
    let tokens = completion_tokens.or_else(|| {
        if final_content.is_empty() {
            None
        } else {
            Some((final_content.len() / 4).max(1) as u64)
        }
    });
    let tokens_per_sec = tokens.map(|t| t as f64 / total.as_secs_f64());
    Completion {
        content: final_content,
        reasoning_content,
        tool_calls: tool_calls.into_values().collect(),
        finish_reason,
        metrics: Metrics {
            ttft_ms: ttft.map(|d| d.as_secs_f64() * 1000.0),
            total_ms: total.as_secs_f64() * 1000.0,
            tokens_per_sec,
            prompt_tokens,
            cached_tokens,
            completion_tokens,
            reasoning_tokens,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Ports;
    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use futures_util::stream;
    use std::path::PathBuf;

    fn test_config(base_url: &str) -> Config {
        Config {
            model: Some("test-model".into()),
            base_url: Some(base_url.into()),
            api_key: Some("sk-test".into()),
            engine_type: None,
            launch_command: None,
            stop_command: None,
            max_context: None,
            ports: Ports::default(),
            max_concurrent_tasks: 1,
            tool_prefix: String::new(),
            searxng_url: None,
            brave_api_key: None,
            openalex_email: None,
            openalex_api_key: None,
            boot_timeout_secs: 180,
            probe_budget: 4,
            dgi_reject_margin: 1.5,
            dgi_bypass_warm_session: false,
            reasoning_open_tag: concat!("<th", "ink>").to_string(),
            reasoning_close_tag: concat!("</th", "ink>").to_string(),
            cache_hit_yield_ratio: 0.20,
            min_eviction_detect_tokens: 13056,
            turn_timeout_secs: 600,
            max_turn_reasoning_tokens: 16384,
            state_dir: PathBuf::from("/tmp/castor-test"),
        }
    }

    fn msg() -> Message {
        Message {
            role: "user".into(),
            content: "hi".into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    async fn start(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn sse_response(frames: &[&str]) -> axum::response::Response {
        let chunks: Vec<axum::body::Bytes> = frames
            .iter()
            .map(|f| axum::body::Bytes::from(format!("data: {f}\n\n")))
            .collect();
        Body::from_stream(stream::iter(chunks).map(Ok::<_, std::io::Error>)).into_response()
    }

    async fn stream_handler() -> axum::response::Response {
        sse_response(&[
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{"content":" world"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
    }

    async fn tools_handler() -> axum::response::Response {
        sse_response(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"ci"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\":\"Paris\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ])
    }

    async fn metrics_handler() -> axum::response::Response {
        sse_response(&[
            r#"{"choices":[{"delta":{"content":"one two three four five"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"completion_tokens":5}}"#,
            "[DONE]",
        ])
    }

    async fn err400_handler() -> (StatusCode, &'static str) {
        (
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"bad model"}}"#,
        )
    }

    async fn err500_handler() -> (StatusCode, &'static str) {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":{"message":"boom"}}"#,
        )
    }

    async fn split_handler() -> axum::response::Response {
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n";
        let bytes = line.as_bytes();
        let mid = bytes.len() / 2;
        let a = axum::body::Bytes::copy_from_slice(&bytes[..mid]);
        let b = axum::body::Bytes::copy_from_slice(&bytes[mid..]);
        let s = stream::iter(vec![a, b])
            .map(|c| async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                Ok::<_, std::io::Error>(c)
            })
            .buffered(2);
        Body::from_stream(s).into_response()
    }

    async fn non_stream_handler() -> Json<Value> {
        Json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "pong",
                    "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "{}"}}]
                },
                "finish_reason": "stop"
            }],
            "usage": {"completion_tokens": 7}
        }))
    }

    #[tokio::test]
    async fn streamed_deltas_assemble_into_one_message() {
        let app = Router::new().route("/chat/completions", post(stream_handler));
        let base = start(app).await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let out = client.chat(&[msg()], &[], true, None).await.unwrap();
        assert_eq!(out.content, "Hello world");
        assert_eq!(out.finish_reason.as_deref(), Some("stop"));
        assert!(out.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn tool_calls_assembled_across_fragments() {
        let app = Router::new().route("/chat/completions", post(tools_handler));
        let base = start(app).await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let out = client.chat(&[msg()], &[], true, None).await.unwrap();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].id, "call_1");
        assert_eq!(out.tool_calls[0].name, "get_weather");
        assert_eq!(out.tool_calls[0].arguments, r#"{"city":"Paris"}"#);
        assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn ttft_and_velocity_present() {
        let app = Router::new().route("/chat/completions", post(metrics_handler));
        let base = start(app).await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let out = client.chat(&[msg()], &[], true, None).await.unwrap();
        assert!(out.metrics.ttft_ms.is_some());
        assert!(out.metrics.tokens_per_sec.is_some());
        assert!(out.metrics.total_ms > 0.0);
        assert_eq!(out.metrics.completion_tokens, Some(5));
    }

    #[tokio::test]
    async fn upstream_http_error_returns_status_and_body() {
        let app = Router::new().route("/chat/completions", post(err400_handler));
        let base = start(app).await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let err = client.chat(&[msg()], &[], true, None).await.unwrap_err();
        match err {
            EngineError::Http { status, body } => {
                assert_eq!(status, 400);
                assert!(body.contains("bad model"), "{body}");
            }
            other => panic!("expected Http error, got {other:?}"),
        }

        let app = Router::new().route("/chat/completions", post(err500_handler));
        let base = start(app).await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let err = client.chat(&[msg()], &[], true, None).await.unwrap_err();
        match err {
            EngineError::Http { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("boom"), "{body}");
            }
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sse_json_split_across_chunks_parses() {
        let app = Router::new().route("/chat/completions", post(split_handler));
        let base = start(app).await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let out = client.chat(&[msg()], &[], true, None).await.unwrap();
        assert_eq!(out.content, "Hello");
        assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn connection_refused_is_clean_err() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let client = EngineClient::from_config(&test_config(&format!("http://{addr}"))).unwrap();
        let err = client.chat(&[msg()], &[], true, None).await.unwrap_err();
        assert!(matches!(err, EngineError::Connect { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn non_streaming_completion() {
        let app = Router::new().route("/chat/completions", post(non_stream_handler));
        let base = start(app).await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let out = client.chat(&[msg()], &[], false, None).await.unwrap();
        assert_eq!(out.content, "pong");
        assert_eq!(out.finish_reason.as_deref(), Some("stop"));
        assert_eq!(out.metrics.completion_tokens, Some(7));
        assert_eq!(out.tool_calls[0].name, "f");
    }

    #[test]
    fn missing_config_fields_are_errors() {
        let mut cfg = test_config("http://127.0.0.1:1");
        cfg.base_url = None;
        assert!(matches!(
            EngineClient::from_config(&cfg),
            Err(EngineError::MissingConfig { field: "base_url" })
        ));
        let mut cfg = test_config("http://127.0.0.1:1");
        cfg.model = None;
        assert!(matches!(
            EngineClient::from_config(&cfg),
            Err(EngineError::MissingConfig { field: "model" })
        ));
    }

    // ------------------------------------------------------------------
    // reasoning_effort payload shape. A capturing handler records the
    // received request body; the test asserts on exactly what was
    // serialized.
    // ------------------------------------------------------------------

    fn assert_effort_fields(body: &Value, effort: Option<&str>) {
        let ct = body.get("chat_template_kwargs");
        let tl = body.get("reasoning_effort");
        match effort {
            Some(e) => {
                assert_eq!(
                    ct,
                    Some(&json!({ "reasoning_effort": e })),
                    "chat_template_kwargs must contain reasoning_effort: {body}"
                );
                assert!(
                    tl.is_none(),
                    "top-level reasoning_effort should not be sent: {body}"
                );
            }
            None => {
                assert!(
                    ct.is_none() && tl.is_none(),
                    "absent effort must serialize no reasoning fields: {body}"
                );
            }
        }
    }

    /// Echo the serialized request body back inside the completion's
    /// `content` field, so a test can recover exactly what the client sent
    /// (the client parses `content` as a string, which we then re-parse).
    async fn echo_request_body(Json(body): Json<Value>) -> Json<Value> {
        Json(json!({
            "choices": [{
                "message": { "content": serde_json::to_string(&body).unwrap() },
                "finish_reason": "stop"
            }]
        }))
    }

    /// Start an echo server; return its base URL.
    async fn start_echo_server() -> String {
        let app = Router::new().route("/chat/completions", post(echo_request_body));
        start(app).await
    }

    /// Recover the request body the client sent (re-parsed from the echo).
    fn sent_body(out: &Completion) -> Value {
        serde_json::from_str(&out.content).expect("echoed request body must be valid JSON")
    }

    #[tokio::test]
    async fn effort_serialized_as_chat_template_kwargs() {
        let base = start_echo_server().await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let out = client
            .chat(&[msg()], &[], false, Some("xhigh"))
            .await
            .unwrap();
        let sent = sent_body(&out);
        assert_effort_fields(&sent, Some("xhigh"));
    }

    #[tokio::test]
    async fn absent_effort_serializes_no_effort_fields() {
        let base = start_echo_server().await;
        let client = EngineClient::from_config(&test_config(&base)).unwrap();
        let out = client.chat(&[msg()], &[], false, None).await.unwrap();
        let sent = sent_body(&out);
        assert_effort_fields(&sent, None);
    }

    #[test]
    fn multimodal_visual_inspection_serializes_image_url() {
        let m = Message {
            role: "user".into(),
            content: "[Visual Inspection Attachment: data:image/png;base64,iVBORw0KGgo=]".into(),
            tool_calls: vec![],
            tool_call_id: None,
        };
        let json = m.to_json();
        assert_eq!(json["role"], "user");
        let parts = json["content"].as_array().expect("must be content array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn multimodal_visual_inspection_with_whitespace_and_newline() {
        let m = Message {
            role: "user".into(),
            content: "  [Visual Inspection Attachment: data:image/jpeg;base64,abc123==] \r\n"
                .into(),
            tool_calls: vec![],
            tool_call_id: None,
        };
        let json = m.to_json();
        let parts = json["content"].as_array().expect("must be content array");
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/jpeg;base64,abc123=="
        );
    }

    #[test]
    fn multimodal_malformed_retains_string_content() {
        let m = Message {
            role: "user".into(),
            content: "[Visual Inspection Attachment: not-an-image]".into(),
            tool_calls: vec![],
            tool_call_id: None,
        };
        let json = m.to_json();
        assert!(json["content"].is_string());
    }

    #[test]
    fn test_extract_fallback_qwen_xml_single_param() {
        let raw = "I will check the files.\n\n<tool_call>\n<function=bash>\n<parameter=command>\ncargo test config\n</parameter>\n</function>\n</tool_call>";
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "I will check the files.");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "bash");
        assert_eq!(tcs[0].arguments, r#"{"command":"cargo test config"}"#);
    }

    #[test]
    fn test_extract_fallback_qwen_xml_multi_param() {
        let raw = "<tool_call>\n<function=edit_file>\n<parameter=path>\nsrc/config.rs\n</parameter>\n<parameter=old_str>\nlet a = 1;\n</parameter>\n<parameter=new_str>\nlet a = 2;\n</parameter>\n</function>\n</tool_call>";
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "edit_file");
        let parsed: Value = serde_json::from_str(&tcs[0].arguments).unwrap();
        assert_eq!(parsed["path"], "src/config.rs");
        assert_eq!(parsed["old_str"], "let a = 1;");
        assert_eq!(parsed["new_str"], "let a = 2;");
    }

    #[test]
    fn test_extract_fallback_qwen_xml_parallel() {
        let raw = "<tool_call>\n<function=read_file>\n<parameter=path>\nsrc/lib.rs\n</parameter>\n</function>\n</tool_call>\n<tool_call>\n<function=read_file>\n<parameter=path>\nsrc/main.rs\n</parameter>\n</function>\n</tool_call>";
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "");
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0].name, "read_file");
        assert_eq!(tcs[0].index, 0);
        assert_eq!(tcs[1].name, "read_file");
        assert_eq!(tcs[1].index, 1);
    }

    #[test]
    fn test_extract_fallback_hermes_json() {
        let raw = "Executing now:\n<tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"cargo test\"}}\n</tool_call>";
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "Executing now:");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "bash");
        assert_eq!(tcs[0].arguments, r#"{"command":"cargo test"}"#);
    }

    #[test]
    fn test_assemble_populates_fallback_tool_calls() {
        let raw = "<tool_call>\n<function=bash>\n<parameter=command>\necho 1\n</parameter>\n</function>\n</tool_call>";
        let completion = assemble(
            raw,
            None,
            BTreeMap::new(),
            Some("stop".into()),
            None,
            None,
            None,
            None,
            None,
            Instant::now(),
        );
        assert_eq!(completion.content, "");
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].name, "bash");
        assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn test_extract_fallback_numeric_and_boolean_string_params_not_coerced() {
        let raw = r#"<tool_call>
<function=edit_file>
<parameter=path>404.html</parameter>
<parameter=target_content>true</parameter>
<parameter=replacement_content>123</parameter>
<parameter=overwrite>false</parameter>
<parameter=start_line>42</parameter>
</function>
</tool_call>"#;
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "edit_file");
        let parsed: Value = serde_json::from_str(&tcs[0].arguments).unwrap();
        // Path, target_content, and replacement_content must remain strings!
        assert_eq!(parsed["path"], "404.html");
        assert_eq!(parsed["target_content"], "true");
        assert_eq!(parsed["replacement_content"], "123");
        // Overwrite must be boolean false
        assert_eq!(parsed["overwrite"], false);
        // Start_line must be integer 42
        assert_eq!(parsed["start_line"], 42);
    }

    #[test]
    fn test_extract_fallback_ignores_hypothetical_tool_call_inside_think() {
        let raw = r#"<think>
Should I run <tool_call><function=bash><parameter=command>rm -rf /</parameter></function></tool_call>? No, that would destroy everything.
</think>
I will list the directory:
<tool_call>
<function=bash>
<parameter=command>ls -la</parameter>
</function>
</tool_call>"#;
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        // The hypothetical tool call inside <think> must NOT be extracted!
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "bash");
        assert_eq!(tcs[0].arguments, r#"{"command":"ls -la"}"#);
        // The <think> block must be preserved in the stripped output
        assert!(stripped.contains("<think>"));
        assert!(stripped.contains("rm -rf /"));
        assert!(stripped.contains("</think>"));
        assert!(!stripped.contains("ls -la"));
    }

    #[test]
    fn test_extract_fallback_openai_nested_function_format() {
        let raw = r#"<tool_call>
{"function": {"name": "read_file", "arguments": {"path": "Cargo.toml", "start_line": 1}}}
</tool_call>"#;
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "read_file");
        let parsed: Value = serde_json::from_str(&tcs[0].arguments).unwrap();
        assert_eq!(parsed["path"], "Cargo.toml");
        assert_eq!(parsed["start_line"], 1);
    }

    #[test]
    fn test_extract_fallback_hermes_arguments_as_escaped_json_string() {
        let raw = r#"<tool_call>
{"name": "bash", "arguments": "{\"command\": \"pwd\"}"}
</tool_call>"#;
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "bash");
        assert_eq!(tcs[0].arguments, r#"{"command": "pwd"}"#);
    }

    #[test]
    fn test_extract_fallback_multiline_bash_with_nested_quotes_and_xml_tags() {
        let script = "python3 -c \"import sys; print('<item>ok</item>')\"\ncat << 'EOF' > test.py\ndef run():\n    return 'hello'\nEOF\npython3 test.py";
        let raw = format!(
            "<tool_call>\n<function=bash>\n<parameter=command>\n{script}\n</parameter>\n</function>\n</tool_call>"
        );
        let (stripped, tcs) = extract_fallback_tool_calls(&raw);
        assert_eq!(stripped, "");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].name, "bash");
        let parsed: Value = serde_json::from_str(&tcs[0].arguments).unwrap();
        assert_eq!(parsed["command"], script);
    }

    #[test]
    fn test_extract_fallback_mixed_qwen_xml_and_hermes_json() {
        let raw = r#"<tool_call>
<function=read_file>
<parameter=path>src/lib.rs</parameter>
</function>
</tool_call>
<tool_call>
{"name": "read_file", "arguments": {"path": "src/main.rs"}}
</tool_call>"#;
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        assert_eq!(stripped, "");
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0].name, "read_file");
        assert_eq!(tcs[0].index, 0);
        let parsed0: Value = serde_json::from_str(&tcs[0].arguments).unwrap();
        assert_eq!(parsed0["path"], "src/lib.rs");

        assert_eq!(tcs[1].name, "read_file");
        assert_eq!(tcs[1].index, 1);
        let parsed1: Value = serde_json::from_str(&tcs[1].arguments).unwrap();
        assert_eq!(parsed1["path"], "src/main.rs");
    }

    #[test]
    fn test_extract_fallback_malformed_unclosed_tags_graceful() {
        let raw = "<tool_call><function=bash><parameter=command>echo 1";
        let (stripped, tcs) = extract_fallback_tool_calls(raw);
        // Truncated / unclosed must not panic and must return empty tool calls
        assert_eq!(stripped, raw);
        assert!(tcs.is_empty());
    }
}
