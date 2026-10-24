//! Serving-engine policy selected by the existing engine_type setting.

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
        match config.engine_type.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("llama.cpp") | Some("llamacpp") | Some("llama_cpp") | Some("llama") | Some("bonsai") => {
                Self::LlamaCpp
            }
            Some("vllm") | Some("hyperqwen") => Self::Vllm,
            Some("generic") | Some("openai") | Some("sglang") | Some("ollama") | Some("lmstudio") => {
                Self::Generic
            }
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
