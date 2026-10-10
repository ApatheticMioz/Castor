//! Serving-engine policy selected by the existing engine_type setting.

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Legacy,
    LlamaCpp,
}

impl Backend {
    pub(crate) fn from_config(config: &Config) -> Self {
        if config.engine_type.as_deref() == Some("llama.cpp") {
            Self::LlamaCpp
        } else {
            Self::Legacy
        }
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
