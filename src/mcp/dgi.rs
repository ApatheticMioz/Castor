//! DGI Gatekeeper — Decomposition Granularity Index.
//!
//! Classifies dispatch prompts with a serving-engine probe.
//!
//! The probe comes in two shapes, and they are intentionally not comparable in
//! latency: the vLLM path is a single-token guided_choice completion pass,
//! while the llama.cpp path is a schema-constrained JSON chat decode, whose
//! duration scales with decode speed on CPU models. Both shapes share one
//! request-builder / verdict-decoder core ([`probe_request`],
//! [`decode_verdict`]) so the decision rule stays DRY and engine-agnostic.

use serde_json::json;

/// Shared classification rubric for the DGI gatekeeper probe.
///
/// Emphasizes cohesion (one coherent concern within a single subsystem, however
/// many files it spans) versus unrelated multi-subsystem overhauls, so raw
/// prompt length is not the deciding signal. The compact contrastive
/// few-shot examples pin down the two failure modes of length-based
/// classifiers: a long-but-cohesive task must be ADMITted, a short-but-sprawly
/// multi-subsystem chain must be flagged OVERLOADED.
const CLASSIFY_RUBRIC: &str = concat!(
    "You are a decomposition gatekeeper. Classify the task below. ",
    "ADMIT: a cohesive, bounded single-subsystem engineering task — one coherent ",
    "concern, even if it touches several files within that one subsystem. ",
    "OVERLOADED: an unrelated multi-subsystem overhaul — a monolithic 'mega-prompt' ",
    "chaining several distinct, independent subsystems (e.g. redesign A, rewrite B, ",
    "re-architect C) with no single coherent goal. ",
    "The deciding test is cohesion: if all the work serves one coherent goal within ",
    "one subsystem it is ADMIT; if it chains unrelated subsystems it is OVERLOADED. ",
    "Examples: ",
    "ADMIT — 'Add retry-with-backoff to the DGI probe in src/mcp/dgi.rs and thread the ",
    "new config knob through src/config.rs; add unit tests and run cargo test.' ",
    "OVERLOADED — 'Rewrite the config loader, re-architect the engine client onto a new ",
    "wire protocol, and rework the task registry schema, all in one dispatch.' ",
);

/// Single-token guided-choice vocabulary for the DGI probe.
const VERDICT_ADMIT: &str = "ADMIT";
const VERDICT_OVERLOADED: &str = "OVERLOADED";

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

/// Turn the probe's first decoded token into a verdict (used only when an
/// endpoint does not return `top_logprobs`). Rejects empty or unrecognized
/// text rather than silently admitting.
fn verdict_from_text(token: &str) -> Result<DgiVerdict, String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err("DGI probe returned empty completion text".into());
    }
    if is_choice_prefix(trimmed, VERDICT_ADMIT, 2) {
        Ok(DgiVerdict::Admit)
    } else if is_choice_prefix(trimmed, VERDICT_OVERLOADED, 2) {
        Ok(DgiVerdict::Reject(vec![
            "Model-classified monolithic multi-subsystem dispatch (OVERLOADED)".to_string(),
        ]))
    } else {
        Err(format!("unexpected DGI probe completion text: {trimmed:?}"))
    }
}

/// Returns `true` when `token` is a non-empty prefix of `target` of at least
/// `min_len` characters (exact-case first, or full-word case-insensitive).
fn is_choice_prefix(token: &str, target: &str, min_len: usize) -> bool {
    let t = token.trim();
    if t.len() >= min_len && target.starts_with(t) {
        return true;
    }
    t.eq_ignore_ascii_case(target)
}

fn logsumexp(a: Option<f64>, b: f64) -> f64 {
    match a {
        None => b,
        Some(prev) => {
            let m = prev.max(b);
            m + ((prev - m).exp() + (b - m).exp()).ln()
        }
    }
}

/// Extract `(token, logprob)` pairs from either `/v1/completions` map format
/// (`{"AD": -0.01, "OVER": -5.3, ...}`) or Chat Completions array format
/// (`[{"token": " ADMIT", "logprob": -0.01}, ...]`).
fn extract_top_logprobs(top_logprobs: &serde_json::Value) -> Option<Vec<(String, f64)>> {
    if let Some(obj) = top_logprobs.as_object() {
        let pairs: Vec<(String, f64)> = obj
            .iter()
            .filter_map(|(k, v)| v.as_f64().map(|lp| (k.clone(), lp)))
            .collect();
        return if pairs.is_empty() { None } else { Some(pairs) };
    }
    if let Some(arr) = top_logprobs.as_array() {
        let pairs: Vec<(String, f64)> = arr
            .iter()
            .filter_map(|e| {
                let tok = e.get("token")?.as_str()?.to_string();
                let lp = e.get("logprob")?.as_f64()?;
                Some((tok, lp))
            })
            .collect();
        return if pairs.is_empty() { None } else { Some(pairs) };
    }
    None
}

/// Log-odds decision rule: `delta_z = logP(OVERLOADED) - logP(ADMIT)` measured
/// on the probe's first token. Supports multi-token BPE first-token prefixes
/// (`AD`/`OVER`) via `logsumexp`, and handles one-sided top-K truncation using
/// `min(top_logprobs)` as a strict upper bound when the losing choice falls
/// outside top-K.
fn verdict_from_logprobs(
    top_logprobs: &serde_json::Value,
    reject_margin: f64,
) -> Result<DgiVerdict, String> {
    let pairs = extract_top_logprobs(top_logprobs)
        .ok_or_else(|| "top_logprobs empty or unrecognized format".to_string())?;

    let mut p_admit: Option<f64> = None;
    let mut p_over: Option<f64> = None;
    let mut floor_lp = f64::INFINITY;

    for (tok, lp) in &pairs {
        floor_lp = floor_lp.min(*lp);
        if is_choice_prefix(tok, VERDICT_ADMIT, 2) {
            p_admit = Some(logsumexp(p_admit, *lp));
        } else if is_choice_prefix(tok, VERDICT_OVERLOADED, 2) {
            p_over = Some(logsumexp(p_over, *lp));
        }
    }

    // Fallback for tokenizers that split choices at a 1-char first token ("A"/"O").
    if p_admit.is_none() || p_over.is_none() {
        for (tok, lp) in &pairs {
            let t = tok.trim();
            if p_admit.is_none() && t == "A" {
                p_admit = Some(*lp);
            } else if p_over.is_none() && t == "O" {
                p_over = Some(*lp);
            }
        }
    }

    match (p_admit, p_over) {
        (Some(a), Some(o)) => {
            let delta_z = o - a;
            if delta_z > reject_margin {
                Ok(DgiVerdict::Reject(vec![format!(
                    "Model-classified monolithic multi-subsystem dispatch (OVERLOADED, log-odds margin {delta_z:.3} > {reject_margin})"
                )]))
            } else if delta_z.abs() <= reject_margin {
                Ok(DgiVerdict::Review(1))
            } else {
                Ok(DgiVerdict::Admit)
            }
        }
        // Only ADMIT appeared in top-K: OVERLOADED logprob is strictly <= floor_lp.
        (Some(a), None) if (floor_lp - a) < -reject_margin => Ok(DgiVerdict::Admit),
        // Only OVERLOADED appeared in top-K: ADMIT logprob is strictly <= floor_lp.
        (None, Some(o)) if (o - floor_lp) > reject_margin => Ok(DgiVerdict::Reject(vec![format!(
            "Model-classified monolithic multi-subsystem dispatch (OVERLOADED, lower-bound margin {:.3} > {reject_margin})",
            o - floor_lp
        )])),
        _ => Err("top_logprobs missing probe choices".into()),
    }
}

/// Parse the probe response: log-odds margin when `top_logprobs` is present,
/// decoded text otherwise.
fn decode_verdict(val: &serde_json::Value, reject_margin: f64) -> Result<DgiVerdict, String> {
    let choice = &val["choices"][0];
    if let Some(lps) = choice["logprobs"]["top_logprobs"].as_array() {
        let first = lps.first().ok_or("empty top_logprobs")?;
        verdict_from_logprobs(first, reject_margin)
    } else {
        let text = choice["text"].as_str().unwrap_or("");
        verdict_from_text(text)
    }
}

/// The one request builder for both probe shapes (the only DRY seam):
/// the guided-choice completion body or the schema-constrained chat body.
fn probe_request(
    probe_shape: crate::engine::backend::ProbeShape,
    model: &str,
    prompt: &str,
) -> serde_json::Value {
    match probe_shape {
        crate::engine::backend::ProbeShape::GuidedChoiceCompletion => json!({
            "model": model,
            "prompt": format!(
                "\u{3c}|im_start|\u{3e}system\n{CLASSIFY_RUBRIC}Answer with only ADMIT or OVERLOADED.\u{3c}|im_end|\u{3e}\n\u{3c}|im_start|\u{3e}user\n{prompt}\u{3c}|im_end|\u{3e}\n\u{3c}|im_start|\u{3e}assistant\n\u{3c}think\u{3e}\n\u{3c}/think\u{3e}\n"
            ),
            "max_tokens": 1,
            "temperature": 0.0,
            "logprobs": 20,
            "guided_choice": [VERDICT_ADMIT, VERDICT_OVERLOADED],
        }),
        crate::engine::backend::ProbeShape::JsonSchemaChat => json!({
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
                "type": "object", "properties": {"verdict": {"type": "string", "enum": [VERDICT_ADMIT, VERDICT_OVERLOADED]}},
                "required": ["verdict"], "additionalProperties": false
            }}
        }),
    }
}

/// Decode the probe response for either shape: the guided-choice shape reads
/// the first decoded token (with a logprobs margin when available); the chat
/// shape reads the JSON verdict field.
fn decode_probe_response(
    probe_shape: crate::engine::backend::ProbeShape,
    val: &serde_json::Value,
    reject_margin: f64,
) -> Result<DgiVerdict, String> {
    match probe_shape {
        crate::engine::backend::ProbeShape::GuidedChoiceCompletion => {
            decode_verdict(val, reject_margin)
        }
        crate::engine::backend::ProbeShape::JsonSchemaChat => {
            if val["choices"][0]["finish_reason"] == "length" {
                return Err("dispatch gate response exceeded its token limit".into());
            }
            let content = val["choices"][0]["message"]["content"]
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
    }
}

/// Query the serving engine for a single-token logit probe.
///
/// Uses the vLLM guided_choice completion format with a one-token output
/// budget and `logprobs: 2` so the verdict is a calibrated log-odds margin.
pub async fn evaluate_model_probe(
    base_url: &str,
    model: &str,
    prompt: &str,
    reject_margin: f64,
    http: &reqwest::Client,
) -> Result<DgiVerdict, String> {
    let url = format!("{}/completions", base_url.trim_end_matches('/'));
    let req_body = probe_request(
        crate::engine::backend::ProbeShape::GuidedChoiceCompletion,
        model,
        prompt,
    );
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
    decode_probe_response(
        crate::engine::backend::ProbeShape::GuidedChoiceCompletion,
        &val,
        reject_margin,
    )
}

/// Query the configured serving engine using its capability-selected probe
/// format, retrying once on transient socket/5xx blips before failing fast.
pub async fn evaluate_configured_model_probe(
    config: &crate::config::Config,
    prompt: &str,
    http: &reqwest::Client,
) -> Result<DgiVerdict, String> {
    use crate::engine::backend::{Backend, ProbeShape, api_url};
    let model = config.model.as_deref().ok_or("missing model in config")?;
    // Route on the engine's probe_shape capability (the single source of truth
    // for the wire format) rather than on one backend's identity: any serving
    // engine that advertises GuidedChoiceCompletion takes the single-token
    // /completions path, and any that advertises JsonSchemaChat takes the
    // schema-constrained chat decode.
    let shape = Backend::from_config(config).probe_shape();
    let url = match shape {
        ProbeShape::GuidedChoiceCompletion => api_url(config, "completions")?,
        ProbeShape::JsonSchemaChat => api_url(config, "chat/completions")?,
    };
    let payload = probe_request(shape, model, prompt);

    let mut attempt = 0;
    loop {
        let mut request = http.post(&url).json(&payload);
        if let Some(key) = &config.api_key {
            request = request.bearer_auth(key);
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .map_err(|e| format!("reading upstream HTTP {status} body: {e}"))?;
                if (status.as_u16() == 502 || status.as_u16() == 503 || status.as_u16() == 504)
                    && attempt == 0
                {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                if !status.is_success() {
                    return Err(format!("upstream HTTP {status}: {body}"));
                }
                let val: serde_json::Value = serde_json::from_str(&body)
                    .map_err(|e| format!("malformed probe response: {e}"))?;
                return decode_probe_response(shape, &val, config.dgi_reject_margin);
            }
            Err(e) if attempt == 0 => {
                attempt += 1;
                tracing::warn!(url = %url, "DGI probe transport error ({e}); retrying once");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => return Err(format!("connection to {url} failed: {e}")),
        }
    }
}

/// Warm-session bypass: a session that already has a non-empty persisted
/// message history is a *continuation*, not a fresh dispatch, so it cannot
/// be a newly-sprayed monolithic mega-prompt — admit it without probing.
pub fn warm_session_bypass(
    config: &crate::config::Config,
    state: &crate::state::StateDir,
    session_id: &str,
) -> bool {
    if !config.dgi_bypass_warm_session {
        return false;
    }
    let path = state.sessions().join(session_id).join("messages.json");
    std::fs::metadata(&path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vllm_completions_map_top_logprobs_decodes_bpe_prefixes() {
        let top = json!({
            "AD": -0.0081,
            "ADM": -5.88,
            "ad": -6.38,
            "OVER": -5.3831
        });
        assert_eq!(verdict_from_logprobs(&top, 1.5).unwrap(), DgiVerdict::Admit);

        let top_mon = json!({
            "OVER": -0.1,
            "AD": -2.5
        });
        assert!(verdict_from_logprobs(&top_mon, 1.5).unwrap().is_reject());
    }

    #[test]
    fn vllm_completions_one_sided_top_logprobs_bounds_missing_choice() {
        // Only AD appeared in top-20; floor is -7.13 -> margin <= -7.12 < -1.5 -> Admit.
        let top = json!({
            "AD": -0.0075,
            "ADM": -5.88,
            "The": -7.13
        });
        assert_eq!(verdict_from_logprobs(&top, 1.5).unwrap(), DgiVerdict::Admit);

        // Only OVER appeared in top-20; floor is -6.0 -> margin >= 5.9 > 1.5 -> Reject.
        let top_over = json!({
            "OVER": -0.1,
            "The": -6.0
        });
        assert!(verdict_from_logprobs(&top_over, 1.5).unwrap().is_reject());
    }

    #[test]
    fn text_fallback_rejects_empty_or_garbage_output() {
        assert!(verdict_from_text("").is_err());
        assert!(verdict_from_text("hello world").is_err());
        assert_eq!(verdict_from_text("AD").unwrap(), DgiVerdict::Admit);
        assert!(verdict_from_text("OVER").unwrap().is_reject());
    }

    #[test]
    fn logprob_margin_rejects_overloaded() {
        // OVERLOADED logprob -0.2 is less negative (more probable) than ADMIT -3.0.
        // delta_z = logP(OVERLOADED) - logP(ADMIT) = -0.2 - (-3.0) = 2.8 > 1.5 → REJECT.
        let top = json!([
            {"token": " ADMIT", "logprob": -3.0},
            {"token": " OVERLOADED", "logprob": -0.2}
        ]);
        assert!(verdict_from_logprobs(&top, 1.5).unwrap().is_reject());
    }

    #[test]
    fn logprob_margin_admits_when_admit_dominant() {
        // ADMIT logprob -0.1 is more probable than OVERLOADED -2.0.
        // delta_z = -2.0 - (-0.1) = -1.9. Not > 1.5, |−1.9| > 1.5 → Admit.
        let top = json!([
            {"token": " ADMIT", "logprob": -0.1},
            {"token": " OVERLOADED", "logprob": -2.0}
        ]);
        assert_eq!(verdict_from_logprobs(&top, 1.5).unwrap(), DgiVerdict::Admit);
    }

    #[test]
    fn logprob_margin_tie_is_advisory_review() {
        // Near-tie: delta_z = -0.7 - (-0.5) = -0.2. |−0.2| ≤ 1.5 → Review(1).
        let top = json!([
            {"token": " ADMIT", "logprob": -0.5},
            {"token": " OVERLOADED", "logprob": -0.7}
        ]);
        assert_eq!(
            verdict_from_logprobs(&top, 1.5).unwrap(),
            DgiVerdict::Review(1)
        );
        // Exact boundary: delta_z = -1.5 - 0.0 = -1.5. |−1.5| = 1.5 ≤ 1.5 → Review(1).
        let top = json!([
            {"token": " ADMIT", "logprob": 0.0},
            {"token": " OVERLOADED", "logprob": -1.5}
        ]);
        assert_eq!(
            verdict_from_logprobs(&top, 1.5).unwrap(),
            DgiVerdict::Review(1)
        );
        // Just past: delta_z = 1.6 - 0.0 = 1.6 > 1.5 → Reject.
        let top = json!([
            {"token": " ADMIT", "logprob": -1.6},
            {"token": " OVERLOADED", "logprob": 0.0}
        ]);
        assert!(verdict_from_logprobs(&top, 1.5).unwrap().is_reject());
    }

    #[test]
    fn logprobs_missing_both_choices_errors() {
        let top = json!([{"token": " FOO", "logprob": -0.5}]);
        assert!(verdict_from_logprobs(&top, 1.5).is_err());
    }

    #[test]
    fn decode_verdict_falls_back_to_text_without_logprobs() {
        let val = json!({"choices": [{"text": "ADMIT", "finish_reason": "stop"}]});
        assert_eq!(decode_verdict(&val, 1.5).unwrap(), DgiVerdict::Admit);
        let val = json!({"choices": [{"text": "OVERLOADED", "finish_reason": "stop"}]});
        assert!(decode_verdict(&val, 1.5).unwrap().is_reject());
    }

    #[test]
    fn warm_session_bypass_requires_flag_file_and_content() {
        let dir = std::env::temp_dir().join(format!(
            "castor_dgi_warm_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let state = crate::state::StateDir::new(&dir);
        let _ = state.ensure();
        let loaded = crate::config::load().unwrap();
        let mut config = loaded.config;
        config.state_dir = dir.clone();
        config.dgi_bypass_warm_session = true;

        let sid = "sess_warm";
        let sess_dir = state.sessions().join(sid);
        std::fs::create_dir_all(&sess_dir).unwrap();

        // Disabled flag → never bypasses.
        config.dgi_bypass_warm_session = false;
        std::fs::write(sess_dir.join("messages.json"), "[]").unwrap();
        assert!(!warm_session_bypass(&config, &state, sid));

        // Enabled, file exists and non-empty → bypass.
        config.dgi_bypass_warm_session = true;
        assert!(warm_session_bypass(&config, &state, sid));

        // Enabled, file empty → no bypass.
        std::fs::write(sess_dir.join("messages.json"), "").unwrap();
        assert!(!warm_session_bypass(&config, &state, sid));

        // Enabled, file absent → no bypass.
        let _ = std::fs::remove_file(sess_dir.join("messages.json"));
        assert!(!warm_session_bypass(&config, &state, sid));

        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn probe_server(response: serde_json::Value) -> (String, reqwest::Client) {
        use axum::Json;
        use axum::Router;
        use axum::routing::post;

        let app = Router::new().route(
            "/completions",
            post(move |_body: Json<serde_json::Value>| {
                let r = response.clone();
                async move { Json(r) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), reqwest::Client::new())
    }

    #[tokio::test]
    async fn model_probe_rejects_on_overloaded() {
        let (base_url, http) = probe_server(json!({
            "choices": [{"text": "OVERLOADED", "finish_reason": "stop"}]
        }))
        .await;
        let verdict = evaluate_model_probe(&base_url, "test-model", "mega task", 1.5, &http)
            .await
            .unwrap();
        assert!(verdict.is_reject());
    }

    #[tokio::test]
    async fn model_probe_admits_on_admissible() {
        let (base_url, http) = probe_server(json!({
            "choices": [{"text": "ADMIT", "finish_reason": "stop"}]
        }))
        .await;
        let verdict = evaluate_model_probe(&base_url, "test-model", "single slice", 1.5, &http)
            .await
            .unwrap();
        assert_eq!(verdict, DgiVerdict::Admit);
    }

    #[tokio::test]
    async fn model_probe_requests_logprobs_and_decodes_margin() {
        // logprobs present: the decoded text says ADMIT but the margin says
        // near-tie → the logprob rule wins (Review(1)).
        let (base_url, http) = probe_server(json!({
            "choices": [{
                "text": "ADMIT",
                "finish_reason": "stop",
                "logprobs": {"top_logprobs": [[
                    {"token": " ADMIT", "logprob": -0.5},
                    {"token": " OVERLOADED", "logprob": -0.6}
                ]]}
            }]
        }))
        .await;
        let verdict = evaluate_model_probe(&base_url, "test-model", "task", 1.5, &http)
            .await
            .unwrap();
        assert_eq!(verdict, DgiVerdict::Review(1));
    }

    #[test]
    fn guided_choice_probe_request_uses_integer_logprobs_without_chat_top_logprobs() {
        let req = probe_request(
            crate::engine::backend::ProbeShape::GuidedChoiceCompletion,
            "qwen3.8-27b",
            "fix bug",
        );
        assert_eq!(req["logprobs"].as_u64(), Some(20));
        assert!(
            req.get("top_logprobs").is_none(),
            "completions endpoint must not receive chat-only top_logprobs field"
        );
        assert_eq!(req["guided_choice"], json!(["ADMIT", "OVERLOADED"]));
    }

    /// Fixture-integrity test for the DGI calibration anchors in
    /// `dgi_anchors.json` (regenerated by `scripts/gen_dgi_fixture.py`).
    ///
    /// The anchors are the live model probe's ground-truth labels: 4
    /// audit-verified false positives (bounded single-concern specs that must
    /// be ADMITted) and 3 genuine monoliths (coupled multi-subsystem sprawl that
    /// must be REJECTed). This test asserts fixture *integrity* only — it parses
    /// the file, checks the class taxonomy, confirms the 4/3 split, and verifies
    /// each prompt's character length matches the recorded `expected_len`.
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

    #[test]
    fn single_char_first_token_fallback_decodes_when_no_multi_char_match() {
        let top = json!({
            "A": -0.05,
            "O": -4.0
        });
        assert_eq!(verdict_from_logprobs(&top, 1.5).unwrap(), DgiVerdict::Admit);
    }

    #[tokio::test]
    async fn configured_probe_retries_once_on_transient_503() {
        use axum::Json;
        use axum::Router;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = calls.clone();
        let app = Router::new().route(
            "/v1/completions",
            post(move || {
                let c = calls_ref.fetch_add(1, Ordering::SeqCst);
                async move {
                    if c == 0 {
                        (StatusCode::SERVICE_UNAVAILABLE, "overloaded".to_string()).into_response()
                    } else {
                        Json(json!({
                            "choices": [{
                                "text": "AD",
                                "finish_reason": "length",
                                "logprobs": {"top_logprobs": [{"AD": -0.01, "OVER": -4.5}]}
                            }]
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let loaded = crate::config::load().expect("load default config");
        let mut cfg = loaded.config;
        cfg.base_url = Some(format!("http://{addr}/v1"));
        cfg.model = Some("qwen3.8-27b".into());
        cfg.engine_type = Some("vllm".into());

        let verdict = evaluate_configured_model_probe(&cfg, "fix bug", &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(verdict, DgiVerdict::Admit);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
