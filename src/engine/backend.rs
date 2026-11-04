//! Serving-engine policy selected by the existing engine_type setting.
//!
//! Also hosts [`EngineProfile`] and the discovery probe that fingerprints a
//! running OpenAI-compatible engine via `/v1/models`, `/health`, and
//! `/version`.

use serde_json::Value;

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Legacy,
    LlamaCpp,
    Vllm,
    Generic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReasoningWireFormat {
    /// DeepSeek / llama.cpp style: delta.reasoning_content field
    #[default]
    DeepSeekField,
    /// Thinking tags embedded in content: <think>...</think>
    ThinkingTags,
    /// Standard completion without explicit reasoning stream
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StreamSentinel {
    /// Requires "data: [DONE]" sentinel before EOF
    RequiresDone,
    /// Standard SSE stream termination upon HTTP EOF
    #[default]
    StandardEof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffortSerializer {
    /// Llama.cpp / OpenAI top-level "reasoning_effort": "xhigh" | "high" | "medium" | "low"
    TopLevel,
    /// vLLM "chat_template_kwargs": {"reasoning_effort": effort}
    ChatTemplateKwargs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessKind {
    /// llama.cpp checks `/health` for {"status": "ok"} and `/models`
    LlamaHealth,
    /// vLLM / OpenAI checks `/v1/models` for model presence
    ModelsList,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeShape {
    /// JSON-schema constrained chat completion
    JsonSchemaChat,
    /// Guided-choice /completions
    GuidedChoiceCompletion,
}

/// Pluggable serving-engine capability profile.
///
/// Any OpenAI-compatible serving engine (llama.cpp, vLLM, SGLang, Ollama, LM Studio,
/// TGI, Aphrodite, Mistral, OpenAI) is fully defined by this capability descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub effort_serializer: EffortSerializer,
    pub reasoning_format: ReasoningWireFormat,
    pub stream_sentinel: StreamSentinel,
    pub readiness_kind: ReadinessKind,
    pub probe_shape: ProbeShape,
    pub is_managed: bool,
}

impl Backend {
    pub fn from_config(config: &Config) -> Self {
        match config
            .engine_type
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("llama.cpp") | Some("llamacpp") | Some("llama_cpp") | Some("llama")
            | Some("bonsai") => Self::LlamaCpp,
            Some("vllm") | Some("hyperqwen") => Self::Vllm,
            Some("generic") | Some("openai") | Some("sglang") | Some("ollama")
            | Some("lmstudio") => Self::Generic,
            _ => Self::Legacy,
        }
    }

    /// Resolve the complete capability profile for this backend.
    #[inline]
    pub fn capabilities(self) -> EngineCapabilities {
        match self {
            Self::LlamaCpp => EngineCapabilities {
                effort_serializer: EffortSerializer::TopLevel,
                reasoning_format: ReasoningWireFormat::DeepSeekField,
                stream_sentinel: StreamSentinel::RequiresDone,
                readiness_kind: ReadinessKind::LlamaHealth,
                probe_shape: ProbeShape::JsonSchemaChat,
                is_managed: true,
            },
            Self::Vllm | Self::Legacy => EngineCapabilities {
                effort_serializer: EffortSerializer::ChatTemplateKwargs,
                reasoning_format: ReasoningWireFormat::DeepSeekField,
                stream_sentinel: StreamSentinel::StandardEof,
                readiness_kind: ReadinessKind::ModelsList,
                probe_shape: ProbeShape::GuidedChoiceCompletion,
                is_managed: false,
            },
            Self::Generic => EngineCapabilities {
                effort_serializer: EffortSerializer::TopLevel,
                reasoning_format: ReasoningWireFormat::DeepSeekField,
                stream_sentinel: StreamSentinel::StandardEof,
                readiness_kind: ReadinessKind::ModelsList,
                probe_shape: ProbeShape::GuidedChoiceCompletion,
                is_managed: false,
            },
        }
    }

    /// Whether this backend is llama.cpp or compatible.
    #[inline]
    pub fn is_llama_cpp(self) -> bool {
        matches!(self, Self::LlamaCpp)
    }

    /// Whether this backend uses vLLM conventions.
    #[inline]
    pub fn is_vllm(self) -> bool {
        matches!(self, Self::Vllm | Self::Legacy)
    }

    /// Whether the stream requires a [DONE] sentinel to be considered complete.
    #[inline]
    pub fn requires_done_sentinel(self) -> bool {
        self.capabilities().stream_sentinel == StreamSentinel::RequiresDone
    }

    /// How to format reasoning_effort in chat requests.
    #[inline]
    pub fn effort_serializer(self) -> EffortSerializer {
        self.capabilities().effort_serializer
    }

    /// What readiness check mechanism to use.
    #[inline]
    pub fn readiness_kind(self) -> ReadinessKind {
        self.capabilities().readiness_kind
    }

    /// What shape to use for Decomposition Granularity Index (DGI) probes.
    #[inline]
    pub fn probe_shape(self) -> ProbeShape {
        self.capabilities().probe_shape
    }
}

/// Join an API-relative endpoint while preserving an optional server prefix.
pub(crate) fn api_url(config: &Config, endpoint: &str) -> Result<String, String> {
    let base = config
        .base_url
        .as_deref()
        .ok_or("missing base_url in config")?;
    let url = reqwest::Url::parse(&format!("{}/", base.trim_end_matches('/')))
        .map_err(|e| format!("invalid base_url: {e}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("base_url must be an HTTP(S) API URL without query or fragment".into());
    }
    url.join(endpoint)
        .map(|u| u.to_string())
        .map_err(|e| e.to_string())
}

/// The server root of the configured base URL: the trailing `/v1` (or
/// `/{anything}/v1` prefix) is stripped, leaving the scheme + host + port.
///
/// vLLM exposes `/version` and `/health` at the root, while its OpenAI
/// API lives under `/v1`; llama.cpp serves both `/v1/health` and root
/// `/health`, so candidates are probed in order until one matches.
fn server_root(base_url: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if let Some(idx) = base.rfind("/v1") {
        Some(base[..idx].to_string())
    } else {
        let parsed = reqwest::Url::parse(base).ok()?;
        let mut root = parsed.clone();
        root.set_path("");
        root.set_query(None);
        root.set_fragment(None);
        Some(root.to_string())
    }
}

/// Candidate URLs for the `/models` probe (OpenAI convention).
fn models_candidates(config: &Config) -> Vec<String> {
    let base = match config
        .base_url
        .as_deref()
        .map(|b| b.trim_end_matches('/'))
        .filter(|b| !b.is_empty())
    {
        Some(b) => b.to_string(),
        None => return Vec::new(),
    };
    vec![format!("{base}/models")]
}

/// Candidate URLs for the `/health` probe: llama.cpp serves it under the
/// API prefix and at the server root; vLLM serves it (as an empty 200)
/// only at the root.
fn health_candidates(config: &Config) -> Vec<String> {
    let base = match config
        .base_url
        .as_deref()
        .map(|b| b.trim_end_matches('/'))
        .filter(|b| !b.is_empty())
    {
        Some(b) => b.to_string(),
        None => return Vec::new(),
    };
    let mut out = vec![format!("{base}/health")];
    if let Some(root) = server_root(&base) {
        if !out.iter().any(|u| u == &format!("{root}/health")) {
            out.push(format!("{root}/health"));
        }
        if !out.iter().any(|u| u == &format!("{root}/v1/health")) {
            out.push(format!("{root}/v1/health"));
        }
    }
    out
}

/// Candidate URLs for the vLLM `/version` probe.
fn version_candidates(config: &Config) -> Vec<String> {
    let base = match config
        .base_url
        .as_deref()
        .map(|b| b.trim_end_matches('/'))
        .filter(|b| !b.is_empty())
    {
        Some(b) => b.to_string(),
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(root) = server_root(&base) {
        out.push(format!("{root}/version"));
    }
    out.push(format!("{base}/version"));
    out
}

/// The dynamically detected profile of a running serving engine.
///
/// Produced by [`EngineProfile::discover`]: the backend is identified from
/// wire-level fingerprints (`/v1/models` `owned_by` / `/version` /
/// `/health`), the model id binds to the engine's single served model when
/// the config requests `None`/`"auto"`, and `max_context` adopts the
/// engine-reported `max_model_len` when the config does not pin one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineProfile {
    /// The backend identified by live probe evidence (falls back to the
    /// config-selected backend when no live fingerprint is available).
    pub backend: Backend,
    /// The canonical model id bound to the running engine.
    pub model_id: String,
    /// Engine-reported context window (`max_model_len`) or the
    /// config-pinned value; `None` when neither is available.
    pub max_context: Option<u32>,
    /// Capability profile resolved for `backend`.
    pub capabilities: EngineCapabilities,
    /// Engine-reported version string (vLLM `/version`), when present.
    pub engine_version: Option<String>,
}

impl EngineProfile {
    /// Fingerprint a running engine by probing `/v1/models`, `/health`,
    /// and `/version`.
    ///
    /// Backend identification rules:
    /// - `vllm`: `/v1/models` reports `owned_by: "vllm"` on any entry, or
    ///   `/version` (root or `{base}/version`) returns a JSON `version`.
    /// - `llama.cpp`: `/health` returns JSON `{"status": "ok"}`, or
    ///   `/v1/models` reports `owned_by: "llama.cpp"` on any entry.
    /// - `generic`: fallback when a standard OpenAI wire format is detected
    ///   (a healthy model list) but no engine-specific fingerprint matched.
    ///
    /// Model binding: when `config.model` is `None` or `"auto"`, the profile
    /// binds to the single model reported by `/v1/models` (erroring when the
    /// engine serves zero or multiple distinct models). An explicitly
    /// configured model is bound as-is and validated against the engine in
    /// [`super::lifecycle::EngineLifecycle::canary`].
    pub async fn discover(config: &Config, client: &reqwest::Client) -> Result<Self, String> {
        let mut models_json = None;
        for url in models_candidates(config) {
            if let Ok(json) = fetch_json(client, config, &url).await {
                models_json = Some(json);
                break;
            }
        }

        let mut health_ok = false;
        for url in health_candidates(config) {
            match fetch_json(client, config, &url).await {
                Ok(json) if json.get("status").and_then(Value::as_str) == Some("ok") => {
                    health_ok = true;
                    break;
                }
                _ => continue,
            }
        }

        let mut version = None;
        for url in version_candidates(config) {
            if let Ok(json) = fetch_json(client, config, &url).await {
                version = json
                    .get("version")
                    .and_then(Value::as_str)
                    .map(String::from);
                break;
            }
        }

        let owned_by = |s: &str| {
            models_json
                .as_ref()
                .and_then(|v| v.get("data").and_then(Value::as_array))
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| e.get("owned_by").and_then(Value::as_str) == Some(s))
                })
        };

        let models_list = models_json
            .as_ref()
            .and_then(|v| v.get("data").and_then(Value::as_array))
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e.get("id").and_then(Value::as_str).map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let backend = if owned_by("vllm") || version.is_some() {
            Backend::Vllm
        } else if health_ok || owned_by("llama.cpp") {
            Backend::LlamaCpp
        } else if !models_list.is_empty() {
            Backend::Generic
        } else {
            Backend::from_config(config)
        };

        let model_id = match config.model.as_deref() {
            None | Some("auto") => match models_list.as_slice() {
                [single] => single.clone(),
                [] => {
                    return Err(
                        "cannot auto-bind a model: /v1/models reported no models".into(),
                    )
                }
                many => {
                    let shown = many.join(", ");
                    return Err(format!(
                        "cannot auto-bind a model: engine serves multiple models [{shown}]; \
                         pin CASTOR_MODEL to one of them"
                    ));
                }
            },
            Some(explicit) => explicit.to_string(),
        };

        let mut max_model_len: Option<u32> = None;
        if models_list.iter().any(|m| m == &model_id) {
            for entry in models_json
                .as_ref()
                .and_then(|v| v.get("data").and_then(Value::as_array))
                .into_iter()
                .flatten()
            {
                if entry.get("id").and_then(Value::as_str) == Some(model_id.as_str())
                    && let Some(len) = entry
                        .get("max_model_len")
                        .and_then(Value::as_u64)
                        .and_then(|v| u32::try_from(v).ok())
                {
                    max_model_len = Some(len);
                    break;
                }
            }
        } else if let Some(len) = max_model_len_from_any_entry(models_json.as_ref()) {
            // Engine reports a single `max_model_len` (common on llama.cpp);
            // adopt it even when the requested model id has no per-model value.
            max_model_len = Some(len);
        }

        Ok(Self {
            backend,
            model_id,
            max_context: config.max_context.or(max_model_len),
            capabilities: backend.capabilities(),
            engine_version: version,
        })
    }
}

/// Extract `max_model_len` from the first `/v1/models` entry that reports one.
fn max_model_len_from_any_entry(models_json: Option<&Value>) -> Option<u32> {
    models_json?
        .get("data")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|e| {
            e.get("max_model_len")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        })
}

/// GET `url`, expect a successful response, and parse the JSON body.
async fn fetch_json(
    client: &reqwest::Client,
    config: &Config,
    url: &str,
) -> Result<Value, String> {
    let mut request = client.get(url);
    if let Some(key) = &config.api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("connection to {url} failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("upstream HTTP {status} from {url}"));
    }
    response.json().await.map_err(|e| format!("malformed JSON from {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use serde_json::json;

    fn test_config(base_url: Option<String>, model: Option<String>) -> Config {
        Config {
            model,
            base_url,
            api_key: None,
            engine_type: None,
            launch_command: None,
            stop_command: None,
            max_context: None,
            ports: crate::config::Ports::default(),
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
            tool_spill_bytes: 16384,
            workspace_skeleton_prefix: true,
            state_dir: std::path::PathBuf::from("/tmp/castor-test"),
        }
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    fn vllm_models_json() -> Value {
        json!({
            "object": "list",
            "data": [{
                "id": "qwen3.8-27b",
                "object": "model",
                "owned_by": "vllm",
                "max_model_len": 150000,
            }]
        })
    }

    #[tokio::test]
    async fn discover_vllm_from_owned_by_and_version() {
        let base = serve(
            Router::new()
                .route("/v1/models", get(|| async { Json(vllm_models_json()) }))
                .route("/version", get(|| async { Json(json!({"version": "0.31.0"})) })),
        )
        .await;
        let cfg = test_config(Some(format!("{base}/v1")), Some("auto".into()));
        let profile = EngineProfile::discover(&cfg, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(profile.backend, Backend::Vllm);
        assert_eq!(profile.model_id, "qwen3.8-27b");
        assert_eq!(profile.max_context, Some(150000));
        assert_eq!(profile.engine_version.as_deref(), Some("0.31.0"));
        assert!(profile.capabilities.is_managed == false);
    }

    #[tokio::test]
    async fn discover_vllm_from_version_when_no_models() {
        let base = serve(Router::new().route(
            "/version",
            get(|| async { Json(json!({"version": "0.31.0"})) }),
        ))
        .await;
        let cfg = test_config(Some(format!("{base}/v1")), Some("qwen3.8-27b".into()));
        let profile = EngineProfile::discover(&cfg, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(profile.backend, Backend::Vllm);
        assert_eq!(profile.model_id, "qwen3.8-27b");
        assert_eq!(profile.max_context, None);
    }

    #[tokio::test]
    async fn discover_llama_cpp_from_health() {
        let base = serve(
            Router::new()
                .route("/v1/health", get(|| async { Json(json!({"status": "ok"})) }))
                .route(
                    "/v1/models",
                    get(|| async {
                        Json(json!({"data": [{"id": "fixture", "max_model_len": 65536}]}))
                    }),
                ),
        )
        .await;
        let cfg = test_config(Some(format!("{base}/v1")), None);
        let profile = EngineProfile::discover(&cfg, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(profile.backend, Backend::LlamaCpp);
        assert_eq!(profile.model_id, "fixture");
        assert_eq!(profile.max_context, Some(65536));
    }

    #[tokio::test]
    async fn discover_llama_cpp_from_owned_by() {
        let base = serve(Router::new().route(
            "/v1/models",
            get(|| async {
                Json(json!({
                    "data": [{"id": "fixture", "owned_by": "llama.cpp", "max_model_len": 8192}]
                }))
            }),
        ))
        .await;
        let cfg = test_config(Some(format!("{base}/v1")), Some("fixture".into()));
        let profile = EngineProfile::discover(&cfg, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(profile.backend, Backend::LlamaCpp);
        assert_eq!(profile.model_id, "fixture");
        assert_eq!(profile.max_context, Some(8192));
    }

    #[tokio::test]
    async fn discover_generic_fallback_on_plain_openai_wire() {
        let base = serve(Router::new().route(
            "/v1/models",
            get(|| async { Json(json!({"data": [{"id": "m1", "owned_by": "acme"}]})) }),
        ))
        .await;
        let cfg = test_config(Some(format!("{base}/v1")), Some("m1".into()));
        let profile = EngineProfile::discover(&cfg, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(profile.backend, Backend::Generic);
        assert_eq!(profile.model_id, "m1");
        assert_eq!(profile.max_context, None);
    }

    #[tokio::test]
    async fn discover_auto_bind_fails_on_multiple_models() {
        let base = serve(Router::new().route(
            "/v1/models",
            get(|| async {
                Json(json!({"data": [
                    {"id": "a", "owned_by": "vllm"},
                    {"id": "b", "owned_by": "vllm"}
                ]}))
            }),
        ))
        .await;
        let cfg = test_config(Some(format!("{base}/v1")), None);
        let err = EngineProfile::discover(&cfg, &reqwest::Client::new())
            .await
            .unwrap_err();
        assert!(err.contains("multiple models"), "{err}");
    }

    #[tokio::test]
    async fn discover_keeps_explicit_model_and_pinned_context() {
        let base = serve(
            Router::new()
                .route("/v1/models", get(|| async { Json(vllm_models_json()) }))
                .route("/version", get(|| async { Json(json!({"version": "0.31.0"})) })),
        )
        .await;
        let mut cfg = test_config(Some(format!("{base}/v1")), Some("qwen3.8-27b".into()));
        cfg.max_context = Some(32768);
        let profile = EngineProfile::discover(&cfg, &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(profile.backend, Backend::Vllm);
        assert_eq!(profile.model_id, "qwen3.8-27b");
        assert_eq!(profile.max_context, Some(32768));
    }
}
