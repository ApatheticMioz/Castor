//! DGI Gatekeeper — Decomposition Granularity Index.
//!
//! Classifies dispatch prompts with a serving-engine probe and an advisory
//! offline heuristic.
//!
//! The probe comes in two shapes, and they are intentionally not comparable in
//! latency: the vLLM path is a 2-token guided_choice single forward pass,
//! while the llama.cpp path is a schema-constrained JSON chat decode of up to
//! 64 tokens, whose duration scales with decode speed on CPU models.
//!
//! Domain agnosticism: no repo-specific patterns, no hardcoded file extensions,
//! and no hard regex or length limits.

use serde_json::json;

/// Shared classification rubric for the DGI gatekeeper probe.
///
/// Emphasizes cohesion (one coherent concern within a single subsystem, however
/// many files it spans) versus unrelated multi-subsystem overhauls, so raw
/// prompt length is not the deciding signal.
const CLASSIFY_RUBRIC: &str = concat!(
    "You are a decomposition gatekeeper. Classify the task below. ",
    "ADMIT: a cohesive, bounded single-subsystem engineering task — one coherent ",
    "concern, even if it touches several files within that one subsystem. ",
    "OVERLOADED: an unrelated multi-subsystem overhaul — a monolithic 'mega-prompt' ",
    "chaining several distinct, independent subsystems (e.g. redesign A, rewrite B, ",
    "re-architect C) with no single coherent goal. ",
    "The deciding test is cohesion: if all the work serves one coherent goal within ",
    "one subsystem it is ADMIT; if it chains unrelated subsystems it is OVERLOADED. ",
);

/// Verdict of the DGI Gatekeeper for a dispatch prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DgiVerdict {
    /// Bounded single-concern spec: proceed without commentary.
    Admit,
    /// Advisory DGI note: proceed, but flag for decomposition.
    Review(u32),
    /// The model classified an overloaded dispatch: fail fast.
    Reject(Vec<String>),
}

impl DgiVerdict {
    pub fn is_reject(&self) -> bool {
        matches!(self, Self::Reject(_))
    }

    pub fn score(&self) -> Option<u32> {
        match self {
            Self::Review(s) => Some(*s),
            _ => None,
        }
    }
}

/// Query the serving engine for a 2-token single forward pass logit probe.
///
/// Uses the vLLM guided_choice completion format with a two-token output budget.
pub async fn evaluate_model_probe(
    base_url: &str,
    model: &str,
    prompt: &str,
    http: &reqwest::Client,
) -> Result<DgiVerdict, String> {
    let url = format!("{}/completions", base_url.trim_end_matches('/'));
    let formatted_prompt = format!(
        "\u{3c}|im_start|\u{3e}system\n{CLASSIFY_RUBRIC}Answer with only ADMIT or OVERLOADED.\u{3c}|im_end|\u{3e}\n\u{3c}|im_start|\u{3e}user\n{prompt}\u{3c}|im_end|\u{3e}\n\u{3c}|im_start|\u{3e}assistant\n\u{3c}think\u{3e}\n\u{3c}/think\u{3e}\n"
    );
    let req_body = json!({
        "model": model,
        "prompt": formatted_prompt,
        "max_tokens": 2,
        "temperature": 0.0,
        "guided_choice": ["ADMIT", "OVERLOADED"],
    });

    let resp = http
        .post(&url)
        .json(&req_body)
        .send()
        .await
        .map_err(|e| format!("connection to {url} failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("upstream HTTP {status}: {body}"));
    }
    let val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("malformed JSON response from {url}: {e}"))?;
    let text = val["choices"][0]["text"].as_str().unwrap_or("").trim();

    if text.starts_with("OVER") || text.starts_with("over") || text.starts_with("Over") {
        Ok(DgiVerdict::Reject(vec![
            "Model-classified monolithic multi-subsystem dispatch (OVERLOADED)".to_string(),
        ]))
    } else {
        Ok(DgiVerdict::Admit)
    }
}

/// Probe using the configured backend's request format and authentication.
pub async fn evaluate_configured_model_probe(
    config: &crate::config::Config,
    prompt: &str,
    http: &reqwest::Client,
) -> Result<DgiVerdict, String> {
    use crate::engine::backend::{Backend, ProbeShape, api_url};
    let base = config
        .base_url
        .as_deref()
        .ok_or("missing base_url in config")?;
    let model = config.model.as_deref().ok_or("missing model in config")?;
    // Route on the engine's probe_shape capability (the single source of truth
    // for the wire format) rather than on one backend's identity: any serving
    // engine that advertises GuidedChoiceCompletion takes the 2-token
    // /completions path, and any that advertises JsonSchemaChat takes the
    // schema-constrained chat decode.
    match Backend::from_config(config).probe_shape() {
        ProbeShape::GuidedChoiceCompletion => {
            return evaluate_model_probe(base, model, prompt, http).await;
        }
        ProbeShape::JsonSchemaChat => {}
    }
    let url = api_url(config, "chat/completions")?;
    let mut request = http.post(&url).json(&json!({
        "model": model,
        "messages": [
            {"role": "system", "content": format!("{CLASSIFY_RUBRIC}Return only JSON with a verdict field.")},
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "temperature": 0.0,
        "max_tokens": 64,
        "reasoning_effort": "none",
        "response_format": {"type": "json_object", "schema": {
            "type": "object", "properties": {"verdict": {"type": "string", "enum": ["ADMIT", "OVERLOADED"]}},
            "required": ["verdict"], "additionalProperties": false
        }}
    }));
    if let Some(key) = &config.api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("connection to {url} failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("reading upstream HTTP {status} body: {e}"))?;
    if !status.is_success() {
        return Err(format!("upstream HTTP {status}: {body}"));
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("malformed probe response: {e}"))?;
    if value["choices"][0]["finish_reason"] == "length" {
        return Err("dispatch gate response exceeded its token limit".into());
    }
    let content = value["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("missing dispatch gate content")?;
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ProbeVerdict {
        verdict: String,
    }
    let verdict: ProbeVerdict = serde_json::from_str(content)
        .map_err(|e| format!("malformed dispatch gate verdict: {e}"))?;
    match verdict.verdict.as_str() {
        "ADMIT" => Ok(DgiVerdict::Admit),
        "OVERLOADED" => Ok(DgiVerdict::Reject(vec![
            "Model-classified monolithic multi-subsystem dispatch (OVERLOADED)".into(),
        ])),
        _ => Err("dispatch gate verdict must be ADMIT or OVERLOADED".into()),
    }
}

/// Offline or soft heuristic evaluation: advisory only, NEVER a hard reject.
pub fn evaluate(prompt: &str) -> DgiVerdict {
    // Soft heuristic: advisory review if length exceeds 2,500 chars, but never reject.
    if prompt.len() > 2500 {
        return DgiVerdict::Review(2);
    }
    DgiVerdict::Admit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_prompt_admits() {
        let v = evaluate("Fix off-by-one in src/pruner.rs line 42.");
        assert_eq!(v, DgiVerdict::Admit);
    }

    #[test]
    fn long_prompt_soft_review_never_rejects() {
        let p = "x".repeat(3000);
        let v = evaluate(&p);
        assert!(!v.is_reject(), "soft heuristic must never reject");
        assert_eq!(v, DgiVerdict::Review(2));
    }

    #[tokio::test]
    async fn model_probe_rejects_on_overloaded() {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;

        async fn handler(Json(_body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            Json(json!({
                "choices": [{
                    "text": "OVERLOADED",
                    "finish_reason": "stop"
                }]
            }))
        }

        let app = Router::new().route("/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let http = reqwest::Client::new();
        let base_url = format!("http://{addr}");
        let verdict = evaluate_model_probe(&base_url, "test-model", "mega task", &http)
            .await
            .unwrap();

        assert!(verdict.is_reject());
    }

    #[tokio::test]
    async fn model_probe_admits_on_admissible() {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;

        async fn handler(Json(_body): Json<serde_json::Value>) -> Json<serde_json::Value> {
            Json(json!({
                "choices": [{
                    "text": "ADMIT",
                    "finish_reason": "stop"
                }]
            }))
        }

        let app = Router::new().route("/completions", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let http = reqwest::Client::new();
        let base_url = format!("http://{addr}");
        let verdict = evaluate_model_probe(&base_url, "test-model", "single slice", &http)
            .await
            .unwrap();

        assert_eq!(verdict, DgiVerdict::Admit);
    }

    /// Fixture-integrity test for the DGI calibration anchors in
    /// `dgi_anchors.json` (regenerated by `scripts/gen_dgi_fixture.py`).
    ///
    /// The anchors are the live model probe's ground-truth labels: 4
    /// audit-verified false positives (bounded single-concern specs that must
    /// be ADMITted) and 3 genuine monoliths (coupled multi-subsystem sprawl that
    /// must be REJECTed). This test asserts fixture *integrity* only — it parses
    /// the file, checks the class taxonomy, confirms the 4/3 split, and verifies
    /// each prompt's character length matches the recorded `expected_len`. It
    /// deliberately does NOT assert that the offline heuristic (`evaluate`)
    /// reproduces these verdicts: that heuristic is advisory-only and never
    /// rejects, so its output is not comparable to the model labels.
    #[test]
    fn dgi_anchors_fixture_integrity() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Anchor {
            class: String,
            name: String,
            expected_len: usize,
            prompt: String,
        }

        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fixture {
            #[allow(dead_code)]
            description: String,
            anchors: std::collections::BTreeMap<String, Anchor>,
        }

        const FIXTURE: &str = include_str!("dgi_anchors.json");
        let fixture: Fixture = serde_json::from_str(FIXTURE).expect("dgi_anchors.json must parse");

        // The fixture description promises exactly 7 anchors: 4 FP + 3 MON.
        let total = fixture.anchors.len();
        let fp = fixture.anchors.values().filter(|a| a.class == "FP").count();
        let mon = fixture
            .anchors
            .values()
            .filter(|a| a.class == "MON")
            .count();
        assert_eq!(total, 7, "expected 7 calibration anchors, found {total}");
        assert_eq!(fp, 4, "expected 4 false positives (FP), found {fp}");
        assert_eq!(mon, 3, "expected 3 monoliths (MON), found {mon}");

        // Per-anchor: closed class taxonomy, non-empty prompt, and a character
        // length matching the recorded expected_len. The generator records char
        // counts (not byte counts), so several anchors with non-ASCII would fail
        // a byte-length assertion — use chars().
        for (key, a) in &fixture.anchors {
            assert!(
                matches!(a.class.as_str(), "FP" | "MON"),
                "anchor {key} ({}) has invalid class {:?}",
                a.name,
                a.class
            );
            assert!(
                !a.prompt.trim().is_empty(),
                "anchor {key} ({}) has an empty prompt",
                a.name
            );
            let chars = a.prompt.chars().count();
            assert_eq!(
                chars, a.expected_len,
                "anchor {key} ({}) prompt length drifted: {chars} chars != recorded {}",
                a.name, a.expected_len
            );
        }
    }
}
