//! OpenAI-compatible chat client: streaming SSE and non-streaming.

use std::collections::BTreeMap;
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
        json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": self.arguments,
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
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
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
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub metrics: Metrics,
}

pub struct EngineClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
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
            .build()
            .map_err(|e| EngineError::Connect {
                url: base_url.clone(),
                source: e,
            })?;
        Ok(Self {
            http,
            base_url,
            model,
            api_key: cfg.api_key.clone(),
        })
    }

    /// Send one chat-completion request, consuming the response.
    ///
    /// When `reasoning_effort` is provided, it is serialized directly as
    /// `chat_template_kwargs: {"reasoning_effort": effort}` for local vLLM.
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
            body["chat_template_kwargs"] = json!({ "reasoning_effort": effort });
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
                    return Ok(assemble(
                        &content,
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
        Ok(assemble(
            &content,
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
        Ok(assemble(
            &content,
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

#[allow(clippy::too_many_arguments)] // each argument is a distinct per-turn metric
fn assemble(
    content: &str,
    tool_calls: BTreeMap<usize, ToolCall>,
    finish_reason: Option<String>,
    ttft: Option<std::time::Duration>,
    prompt_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    t0: Instant,
) -> Completion {
    let total = t0.elapsed();
    let tokens = completion_tokens.or_else(|| {
        if content.is_empty() {
            None
        } else {
            Some((content.len() / 4).max(1) as u64)
        }
    });
    let tokens_per_sec = tokens.map(|t| t as f64 / total.as_secs_f64());
    Completion {
        content: content.to_string(),
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
}
